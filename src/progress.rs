//! A `[i/total] name` step counter shared by the translation and
//! verification passes.
//!
//! Verbose mode logs one line per step into the scrollback; an interactive
//! stderr instead gets a single live counter line overwritten in place (the
//! current name is printed *before* the work happens, so a slow step is
//! visible as a pause); non-interactive non-verbose runs stay quiet so
//! stdout remains the only machine-readable output.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

/// How progress is rendered.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// One line per step, kept in the scrollback (`-v`).
    Verbose,
    /// A single overwritten counter line on an interactive terminal;
    /// quiet otherwise.
    Auto,
}

/// A thread-safe `[i/total]` counter; steps may be claimed one at a time
/// (per derivation) or in batches (per verification chunk).
pub struct Progress {
    mode: Mode,
    total: usize,
    counter: AtomicUsize,
}

impl Progress {
    pub fn new(mode: Mode, total: usize) -> Self {
        Progress {
            mode,
            total,
            counter: AtomicUsize::new(0),
        }
    }

    /// Claim the next `n` steps and return the 1-based index of the first.
    fn claim(&self, n: usize) -> usize {
        self.counter.fetch_add(n, Ordering::SeqCst) + 1
    }

    /// Claim one step and show `[i/total] name` for it.
    pub fn step(&self, name: &str) {
        let i = self.claim(1);
        self.render(i, name);
    }

    /// Claim `n` steps at once and show `[i/total] name` for the first of
    /// them, where `name` labels the whole batch.
    pub fn step_many(&self, n: usize, name: &str) {
        let i = self.claim(n);
        self.render(i, name);
    }

    fn render(&self, i: usize, name: &str) {
        let total = self.total;
        match self.mode {
            Mode::Verbose => eprintln!("[{i}/{total}] {name}"),
            Mode::Auto if std::io::stderr().is_terminal() => {
                // \r to column 0, \x1b[2K to clear the line.
                eprint!("\r\x1b[2K[{i}/{total}] {name}");
                let _ = std::io::stderr().flush();
            }
            Mode::Auto => {}
        }
    }

    /// Replace the live counter line with a final `[total/total] done`.
    /// Verbose mode already logged every step, so nothing is printed there.
    pub fn done(&self) {
        let total = self.total;
        if self.mode == Mode::Auto && std::io::stderr().is_terminal() {
            eprintln!("\r\x1b[2K[{total}/{total}] done");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests that claim() hands out contiguous 1-based indices whether steps
    // are taken singly or in batches, by claiming a mix and checking each
    // returned start index.
    #[test]
    fn claim_is_contiguous_across_batch_sizes() {
        let p = Progress::new(Mode::Auto, 250);
        assert_eq!(p.claim(1), 1);
        assert_eq!(p.claim(100), 2);
        assert_eq!(p.claim(100), 102);
        assert_eq!(p.claim(49), 202);
        assert_eq!(p.claim(1), 251);
    }
}
