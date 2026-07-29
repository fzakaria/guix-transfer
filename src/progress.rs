//! Progress display shared by the translation and verification passes,
//! rendered with `superconsole`.
//!
//! Verbose mode logs one plain line per step into the scrollback. Otherwise
//! an interactive stderr gets a live region redrawn from scratch every
//! [`REFRESH_INTERVAL`]: one line per in-flight step (elapsed time + name,
//! oldest step first, as many as the terminal has rows for) above a
//! `[done/total]` counter line. The terminal size is re-read every frame, so
//! the region follows resizes, and on a many-core machine the steps beyond
//! the visible rows collapse into a `+N more in flight` note on the counter
//! line — oldest-first means the longest-running (i.e. stalling) steps are
//! always the visible ones. `SuperConsole::new` refuses non-interactive
//! terminals, so non-verbose non-interactive runs stay quiet and stdout
//! remains the only machine-readable output.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use superconsole::{Component, Dimensions, DrawMode, Line, Lines, SuperConsole};

/// How progress is rendered.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// One line per step, kept in the scrollback (`-v`).
    Verbose,
    /// A live in-flight display on an interactive terminal; quiet otherwise.
    Auto,
}

/// How often the live region is redrawn: each frame re-reads the terminal
/// size and re-renders every line, advancing the elapsed times.
const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

const SECS_PER_MINUTE: u64 = 60;

/// The scrollback line verbose mode prints when a step (or batch) starts.
fn step_label(i: usize, total: usize, name: &str) -> String {
    format!("[{i}/{total}] {name}")
}

/// Compact elapsed rendering for an in-flight line: bare seconds under a
/// minute, then minutes and seconds.
fn fmt_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < SECS_PER_MINUTE {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / SECS_PER_MINUTE, secs % SECS_PER_MINUTE)
    }
}

/// One running step in the shared table. The table is kept in start order,
/// so its head is always the longest-running work.
struct InFlightEntry {
    id: usize,
    started_at: Instant,
    name: String,
}

/// Everything the draw pass reads, shared between the worker threads (which
/// mutate it) and the render ticker (which only reads it).
struct DisplayState {
    total: usize,
    /// Steps finished so far — the `[done/total]` counter.
    completed: AtomicUsize,
    /// In-flight steps in start order.
    active: Mutex<Vec<InFlightEntry>>,
    /// Counter-line label set by batch steps (the verification chunks name
    /// the file batch currently being evaluated).
    batch_label: Mutex<String>,
}

/// The superconsole root component. Stateless: every frame is drawn from
/// scratch off the shared [`DisplayState`], which is what makes resizes and
/// out-of-order step completion trivial.
struct ProgressComponent {
    state: Arc<DisplayState>,
}

impl Component for ProgressComponent {
    fn draw_unchecked(&self, dimensions: Dimensions, mode: DrawMode) -> anyhow::Result<Lines> {
        let completed = self.state.completed.load(Ordering::SeqCst);
        let total = self.state.total;

        // The final frame becomes scrollback: just the counter, marked done
        // only if everything actually finished (an abort keeps the raw count).
        if mode == DrawMode::Final {
            let label = if completed == total { " done" } else { "" };
            return Ok(Lines(vec![Line::sanitized(&format!(
                "[{completed}/{total}]{label}"
            ))]));
        }

        // One line per in-flight step, oldest first, leaving a row for the
        // counter line; steps beyond that surface as an overflow note.
        let active = self.state.active.lock().unwrap();
        let shown = active.len().min(dimensions.height.saturating_sub(1));
        let mut lines = Vec::with_capacity(shown + 1);
        for entry in active.iter().take(shown) {
            lines.push(Line::sanitized(&format!(
                "  {:>4} {}",
                fmt_elapsed(entry.started_at.elapsed()),
                entry.name
            )));
        }

        let overflow = active.len() - shown;
        let label = if overflow > 0 {
            format!("+{overflow} more in flight")
        } else {
            self.state.batch_label.lock().unwrap().clone()
        };
        lines.push(Line::sanitized(&format!("[{completed}/{total}] {label}")));
        Ok(Lines(lines))
    }
}

/// The render ticker owns the `SuperConsole` for the lifetime of one
/// progress phase: draw the component, sleep, repeat; on stop, a final
/// render leaves the closing counter line in the scrollback.
fn run_ticker(state: Arc<DisplayState>, stop: Arc<AtomicBool>) {
    // None when stderr is not an interactive (ANSI-capable) terminal: the
    // non-verbose non-interactive contract is to print nothing at all.
    let Some(mut console) = SuperConsole::new() else {
        return;
    };
    let component = ProgressComponent { state };
    while !stop.load(Ordering::SeqCst) {
        let _ = console.render(&component);
        thread::sleep(REFRESH_INTERVAL);
    }
    let _ = console.finalize(&component);
}

/// A thread-safe step counter with a live display. Steps are claimed either
/// one at a time with an in-flight line ([`Progress::start`], per parallel
/// derivation) or in batches ([`Progress::step_many`], per verification
/// chunk).
pub struct Progress {
    mode: Mode,
    state: Arc<DisplayState>,
    /// Steps claimed so far (in-flight included), for verbose labels.
    started: AtomicUsize,
    ticker_stop: Arc<AtomicBool>,
    ticker: Mutex<Option<thread::JoinHandle<()>>>,
}

/// RAII handle for one in-flight step, returned by [`Progress::start`]: its
/// live line stays up while the handle lives, and dropping the handle (on
/// success and error alike) counts the step completed.
pub struct ActiveStep<'a> {
    progress: &'a Progress,
    id: Option<usize>,
}

impl Progress {
    pub fn new(mode: Mode, total: usize) -> Self {
        let state = Arc::new(DisplayState {
            total,
            completed: AtomicUsize::new(0),
            active: Mutex::new(Vec::new()),
            batch_label: Mutex::new(String::new()),
        });

        // Verbose mode prints plain lines itself; the live region only
        // exists in Auto mode, driven by the ticker thread.
        let ticker_stop = Arc::new(AtomicBool::new(false));
        let ticker = (mode == Mode::Auto).then(|| {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&ticker_stop);
            thread::spawn(move || run_ticker(state, stop))
        });

        Progress {
            mode,
            state,
            started: AtomicUsize::new(0),
            ticker_stop,
            ticker: Mutex::new(ticker),
        }
    }

    /// Begin one step: register its in-flight line (or print the verbose
    /// scrollback line) and return the handle whose drop marks the step
    /// completed.
    pub fn start(&self, name: &str) -> ActiveStep<'_> {
        let i = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        if self.mode == Mode::Verbose {
            eprintln!("{}", step_label(i, self.state.total, name));
            return ActiveStep {
                progress: self,
                id: None,
            };
        }

        self.state.active.lock().unwrap().push(InFlightEntry {
            id: i,
            started_at: Instant::now(),
            name: name.to_string(),
        });
        ActiveStep {
            progress: self,
            id: Some(i),
        }
    }

    /// Claim `n` steps at once, labeled by `name`, counting them completed
    /// immediately. For sequential batch work (the verification chunks),
    /// where per-step handles would add nothing.
    pub fn step_many(&self, n: usize, name: &str) {
        let i = self.started.fetch_add(n, Ordering::SeqCst) + 1;
        if self.mode == Mode::Verbose {
            eprintln!("{}", step_label(i, self.state.total, name));
        } else {
            *self.state.batch_label.lock().unwrap() = name.to_string();
        }
        self.state.completed.fetch_add(n, Ordering::SeqCst);
    }

    /// Close out the display: stop the ticker and wait for its final render,
    /// so the closing `[total/total] done` line lands in the scrollback
    /// before any output that follows. Verbose mode already logged every
    /// step; a non-interactive run has nothing to close.
    pub fn done(&self) {
        self.ticker_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.ticker.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ActiveStep<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let mut active = self.progress.state.active.lock().unwrap();
            if let Some(pos) = active.iter().position(|e| e.id == id) {
                active.remove(pos);
            }
        }
        self.progress.state.completed.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // Identical to done(); covers early returns and error paths, where
        // the final render truthfully reports the partial count.
        self.done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain-text rendering of a draw, for assertions.
    fn drawn_text(
        state: &Arc<DisplayState>,
        dimensions: Dimensions,
        mode: DrawMode,
    ) -> Vec<String> {
        let component = ProgressComponent {
            state: Arc::clone(state),
        };
        component
            .draw_unchecked(dimensions, mode)
            .unwrap()
            .iter()
            .map(|line| line.to_unstyled())
            .collect()
    }

    // Tests the verbose scrollback label for a claimed step by formatting one
    // and comparing the exact string.
    #[test]
    fn step_label_formats_counter_and_name() {
        assert_eq!(step_label(3, 250, "glibc-2.39"), "[3/250] glibc-2.39");
    }

    // Tests the compact elapsed format at the seconds/minutes boundary.
    #[test]
    fn fmt_elapsed_seconds_then_minutes() {
        assert_eq!(fmt_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(fmt_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(fmt_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(fmt_elapsed(Duration::from_secs(61)), "1m01s");
        assert_eq!(fmt_elapsed(Duration::from_secs(600)), "10m00s");
    }

    // Tests that completion is counted when ActiveStep handles drop (in any
    // order) and immediately for step_many batches, by watching the shared
    // completed counter advance.
    #[test]
    fn steps_count_on_drop_and_batch() {
        let p = Progress::new(Mode::Auto, 4);
        let a = p.start("a");
        let b = p.start("b");
        assert_eq!(p.state.completed.load(Ordering::SeqCst), 0);

        drop(b);
        assert_eq!(p.state.completed.load(Ordering::SeqCst), 1);
        drop(a);
        assert_eq!(p.state.completed.load(Ordering::SeqCst), 2);

        p.step_many(2, "c");
        assert_eq!(p.state.completed.load(Ordering::SeqCst), 4);
    }

    // Tests the component draw against the terminal height: with more
    // in-flight steps than rows, only the oldest fill the region (one row
    // reserved for the counter) and the surplus becomes an overflow note;
    // with enough rows, every step gets a line. The draw is a pure function
    // of state and dimensions, so no terminal is involved.
    #[test]
    fn draw_fits_height_and_summarizes_overflow() {
        let p = Progress::new(Mode::Auto, 10);
        let _steps: Vec<_> = (0..5).map(|i| p.start(&format!("drv-{i}"))).collect();

        // 4 rows: 3 oldest steps + the counter line carrying the overflow.
        let cramped = drawn_text(&p.state, Dimensions::new(80, 4), DrawMode::Normal);
        assert_eq!(cramped.len(), 4);
        assert!(cramped[0].ends_with("drv-0"), "{cramped:?}");
        assert!(cramped[2].ends_with("drv-2"), "{cramped:?}");
        assert_eq!(cramped[3], "[0/10] +2 more in flight");

        // Plenty of rows: all 5 steps and no overflow note.
        let roomy = drawn_text(&p.state, Dimensions::new(80, 40), DrawMode::Normal);
        assert_eq!(roomy.len(), 6);
        assert!(roomy[4].ends_with("drv-4"), "{roomy:?}");
        assert_eq!(roomy[5], "[0/10] ");
    }

    // Tests that finishing the oldest step slides the visible window: the
    // next-oldest steps move up and the overflow shrinks.
    #[test]
    fn draw_window_slides_as_oldest_finishes() {
        let p = Progress::new(Mode::Auto, 10);
        let mut steps: Vec<_> = (0..5).map(|i| p.start(&format!("drv-{i}"))).collect();

        drop(steps.remove(0));
        let text = drawn_text(&p.state, Dimensions::new(80, 4), DrawMode::Normal);
        assert!(text[0].ends_with("drv-1"), "{text:?}");
        assert!(text[2].ends_with("drv-3"), "{text:?}");
        assert_eq!(text[3], "[1/10] +1 more in flight");
    }

    // Tests the final frame: a completed run collapses to a single done
    // line, while an aborted run keeps the truthful partial count.
    #[test]
    fn final_frame_reports_done_or_partial() {
        let p = Progress::new(Mode::Auto, 2);
        p.step_many(2, "all of it");
        assert_eq!(
            drawn_text(&p.state, Dimensions::new(80, 10), DrawMode::Final),
            vec!["[2/2] done"]
        );

        let aborted = Progress::new(Mode::Auto, 5);
        aborted.step_many(3, "partial");
        assert_eq!(
            drawn_text(&aborted.state, Dimensions::new(80, 10), DrawMode::Final),
            vec!["[3/5]"]
        );
    }

    // Tests that batch steps label the counter line with the current batch
    // (how the verification chunks surface what is being evaluated).
    #[test]
    fn batch_label_shows_on_counter_line() {
        let p = Progress::new(Mode::Auto, 8);
        p.step_many(4, "chunk-of-files.nix");
        let text = drawn_text(&p.state, Dimensions::new(80, 10), DrawMode::Normal);
        assert_eq!(text, vec!["[4/8] chunk-of-files.nix"]);
    }
}
