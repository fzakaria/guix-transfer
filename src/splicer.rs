//! Bottom-up translation of a Guix derivation graph into Nix derivations.
//!
//! For each derivation, in dependency order:
//!   1. `builtin:download` → `builtin:fetchurl` (drop Guix mirror machinery).
//!   2. Add any `input_srcs` (source files/dirs) to the Nix store, rewriting
//!      embedded store paths in text files.
//!   3. Rewrite every `/gnu/store` reference (input drvs, builder, args, env)
//!      to its already-translated `/nix/store` counterpart.
//!   4. Blank the derivation's own output paths so Nix recomputes them.
//!   5. Emit JSON and register via `nix derivation add`.
//!   6. Record guix→nix mappings (drv path + each output path) for parents.
//!
//! There is deliberately no "bootstrap boundary" or `stdenv` substitution: the
//! Guix seeds are statically-linked downloads, so the whole graph translates
//! organically (see NOTES.md / DESIGN.md §4.2).

use crate::ast::{Derivation, store_path_name};
use crate::graph::DerivationGraph;
use crate::{hash, json, mirrors, net, nixstore};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::sync::LazyLock;

/// A bare `/gnu/store` store-directory constant: `/gnu/store` NOT followed by a
/// `/<hash>-...` path component (i.e. followed by a non-`/` char or end).
static BARE_STORE_DIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/gnu/store([^/]|$)").unwrap());

/// A full Guix store path: `/gnu/store/<32-char base32 hash>-`.
static FULL_STORE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/gnu/store/[0-9a-z]{32}-").unwrap());

/// Guix-specific env vars on `builtin:download` derivations that have no
/// meaning for `builtin:fetchurl` and must be dropped.
const DROP_DOWNLOAD_ENV: &[&str] = &[
    "mirrors",
    "disarchive-mirrors",
    "content-addressed-mirrors",
    "impureEnvVars",
    "preferLocalBuild",
];

pub struct Splicer {
    /// Any Guix store path (drv, output, or source) → its Nix counterpart.
    pub map: HashMap<String, String>,
    /// Staging directory for rewritten sources before `nix-store --add`.
    stage: std::path::PathBuf,
    counter: usize,
    /// Memoised URL reachability probes (`url → ok`, upstream mode only).
    url_cache: HashMap<String, bool>,
    pub verbose: bool,
    /// Fetch download seeds from their original upstream mirrors (with probing)
    /// instead of the Guix content-addressed mirror.
    pub upstream: bool,
    /// In upstream mode, probe candidate URLs before committing to one.
    pub probe: bool,
}

impl Splicer {
    pub fn new() -> Self {
        let stage = std::env::temp_dir().join(format!("guix-transfer-{}", std::process::id()));
        Self {
            map: HashMap::new(),
            stage,
            counter: 0,
            url_cache: HashMap::new(),
            verbose: false,
            upstream: false,
            probe: true,
        }
    }

    /// Translate the whole graph; returns the final (root) Nix `.drv` path.
    pub fn run(&mut self, graph: &DerivationGraph) -> Result<String, String> {
        fs::create_dir_all(&self.stage)
            .map_err(|e| format!("create stage dir {}: {e}", self.stage.display()))?;
        let total = graph.order.len();
        let mut last = String::new();
        for (i, drv_path) in graph.order.iter().enumerate() {
            self.progress(i + 1, total, store_path_name(drv_path));
            last = self.translate_one(drv_path, &graph.derivations[drv_path])?;
        }
        self.progress_done(total);
        Ok(last)
    }

    /// Show which derivation we're on. In verbose mode every step is a line; on
    /// an interactive terminal we instead overwrite a single live counter line
    /// (printing the *current* name up front so a slow `nix`/network call is
    /// visible as a pause). Non-interactive non-verbose runs stay quiet so the
    /// stdout `.drv` path is the only machine-readable output.
    fn progress(&self, i: usize, total: usize, name: &str) {
        if self.verbose {
            eprintln!("[{i}/{total}] {name}");
        } else if std::io::stderr().is_terminal() {
            // \r to column 0, \x1b[2K to clear the line.
            eprint!("\r\x1b[2K[{i}/{total}] {name}");
            let _ = std::io::stderr().flush();
        }
    }

    fn progress_done(&self, total: usize) {
        if !self.verbose && std::io::stderr().is_terminal() {
            eprintln!("\r\x1b[2K[{total}/{total}] done");
        }
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            eprintln!("{msg}");
        }
    }

    fn translate_one(&mut self, drv_path: &str, original: &Derivation) -> Result<String, String> {
        let mut drv = original.clone();

        if drv.builder == "builtin:download" {
            let url = self.choose_download_url(&drv)?;
            self.to_fetchurl(&mut drv, url);
        } else {
            self.translate_input_srcs(&mut drv)?;
        }

        // Rewrite all known store paths in inputs, builder, args, env.
        for input in &mut drv.input_drvs {
            if let Some(nix) = self.map.get(&input.path) {
                input.path = nix.clone();
            }
        }
        drv.builder = self.rewrite_str(&drv.builder);
        for a in &mut drv.args {
            *a = self.rewrite_str(a);
        }
        let output_names: Vec<String> = drv.outputs.iter().map(|o| o.name.clone()).collect();
        for e in &mut drv.env {
            if output_names.contains(&e.key) {
                // Self-reference: blank so Nix fills in the recomputed path.
                e.value = String::new();
            } else {
                e.value = self.rewrite_str(&e.value);
            }
        }

        // Blank our own output paths (Nix recomputes input-addressed ones;
        // fixed-output ones are derived from the hash).
        for o in &mut drv.outputs {
            o.path = String::new();
        }

        self.warn_leftover(drv_path, &drv);

        let value = json::to_nix_json(&drv, drv_path)?;
        let nix_drv = nixstore::derivation_add(&value)?;
        self.log(&format!("  {} -> {}", store_path_name(drv_path), nix_drv));

        // Map the drv path and every output path for parents. `nix derivation
        // show` reports output paths store-relative; re-prefix with the store
        // dir taken from the (full) drv path so downstream string rewrites work.
        self.map.insert(drv_path.to_string(), nix_drv.clone());
        let store_dir = nix_drv
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or("/nix/store");
        let nix_outputs = nixstore::output_paths(&nix_drv)?;
        for out in &original.outputs {
            if let Some(nix_out) = nix_outputs.get(&out.name) {
                let full = if nix_out.starts_with('/') {
                    nix_out.clone()
                } else {
                    format!("{store_dir}/{nix_out}")
                };
                self.map.insert(out.path.clone(), full);
            }
        }
        Ok(nix_drv)
    }

    /// Choose a single URL for a `builtin:download` derivation.
    ///
    /// Default: the Guix content-addressed mirror, keyed by the FOD's own
    /// sha256 — one reliable URL serving every source Guix's CI has seen, which
    /// matters because `builtin:fetchurl` cannot fall back across a list.
    ///
    /// `--upstream` mode instead ranks the original mirror list by host
    /// reliability and probes each (with memoisation), picking the first
    /// reachable one.
    fn choose_download_url(&mut self, drv: &Derivation) -> Result<String, String> {
        if self.upstream {
            let raw_url = drv.env_get("url").unwrap_or("").to_string();
            let candidates = mirrors::candidate_urls(&mirrors::extract_urls(&raw_url));
            if candidates.is_empty() {
                return Err(format!("no usable URL in download env {raw_url:?}"));
            }
            if !self.probe {
                return Ok(candidates[0].clone());
            }
            for url in &candidates {
                let ok = *self
                    .url_cache
                    .entry(url.clone())
                    .or_insert_with(|| net::url_ok(url));
                if ok {
                    return Ok(url.clone());
                }
                self.log(&format!("    unreachable, trying next: {url}"));
            }
            self.log(&format!(
                "    WARNING: none reachable, using {}",
                candidates[0]
            ));
            return Ok(candidates[0].clone());
        }

        // Guix CA mirror, from the (single) fixed output's hash.
        let out = drv
            .outputs
            .first()
            .filter(|o| !o.hash.is_empty())
            .ok_or_else(|| "download derivation has no hashed output".to_string())?;
        // The mirror keys on the source's *file name*, which Guix derives from
        // the original URL basename (e.g. `hello-2.12.tar.gz`), not the FOD
        // output's store-path name (which may be e.g. `hello-source`).
        let name =
            Self::download_file_name(drv).unwrap_or_else(|| store_path_name(&out.path).to_string());
        hash::guix_ca_mirror_url(&name, &out.hash)
    }

    /// The source file name for the Guix CA mirror: the basename of the original
    /// download URL (query/fragment stripped). Returns `None` if no usable URL.
    fn download_file_name(drv: &Derivation) -> Option<String> {
        let raw = drv.env_get("url").unwrap_or("");
        mirrors::extract_urls(raw).into_iter().find_map(|u| {
            let path = u.split(['?', '#']).next().unwrap_or(&u);
            let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            (!base.is_empty()).then(|| base.to_string())
        })
    }

    /// Convert a `builtin:download` derivation in place to `builtin:fetchurl`,
    /// using the already-chosen `url`.
    fn to_fetchurl(&self, drv: &mut Derivation, url: String) {
        let executable = drv.env_get("executable") == Some("1");

        drv.builder = "builtin:fetchurl".to_string();
        drv.system = "builtin".to_string();
        drv.args.clear();
        // Downloads pull their mirror lists from input_srcs; drop them.
        drv.input_srcs.clear();
        drv.input_drvs.clear();

        // Keep only url/out (+ executable); rebuild env cleanly.
        let mut env = vec![crate::ast::EnvVar {
            key: "url".into(),
            value: url,
        }];
        if executable {
            env.push(crate::ast::EnvVar {
                key: "executable".into(),
                value: "1".into(),
            });
        }
        // Preserve the `out` env var (blanked later in the common path).
        if let Some(out) = drv.outputs.first() {
            env.push(crate::ast::EnvVar {
                key: out.name.clone(),
                value: String::new(),
            });
        }
        env.retain(|e| !DROP_DOWNLOAD_ENV.contains(&e.key.as_str()));
        drv.env = env;
    }

    /// Add every `/gnu/store` source to the Nix store, rewriting text content.
    ///
    /// Sources may reference *each other* by absolute path — e.g. a generated
    /// Guile builder script that embeds the path of a sibling `.patch`. We must
    /// add a referenced source (and map it) before rewriting the source that
    /// references it, otherwise the stale `/gnu/store` path survives into the
    /// rewritten file. So resolve in dependency order: repeatedly add whichever
    /// pending sources have all their sibling references already mapped.
    fn translate_input_srcs(&mut self, drv: &mut Derivation) -> Result<(), String> {
        let srcs = std::mem::take(&mut drv.input_srcs);
        let siblings: HashSet<String> = srcs
            .iter()
            .filter(|s| s.starts_with("/gnu/store"))
            .cloned()
            .collect();

        let mut pending: Vec<String> = srcs
            .iter()
            .filter(|s| s.starts_with("/gnu/store") && !self.map.contains_key(*s))
            .cloned()
            .collect();

        while !pending.is_empty() {
            let mut still = Vec::new();
            let mut progressed = false;
            for src in std::mem::take(&mut pending) {
                if self.src_ready(&src, &siblings)? {
                    let nix = self.add_source(&src)?;
                    self.map.insert(src, nix);
                    progressed = true;
                } else {
                    still.push(src);
                }
            }
            pending = still;
            if !progressed {
                // A cycle, or a reference to something outside this drv's srcs:
                // add the rest best-effort (rewriting whatever is mapped).
                for src in std::mem::take(&mut pending) {
                    let nix = self.add_source(&src)?;
                    self.map.insert(src, nix);
                }
            }
        }

        drv.input_srcs = srcs
            .into_iter()
            .map(|s| self.map.get(&s).cloned().unwrap_or(s))
            .collect();
        Ok(())
    }

    /// A source is ready to add once every sibling source it textually
    /// references is already mapped. Directories and binaries are always ready
    /// (we add them verbatim).
    fn src_ready(&self, src: &str, siblings: &HashSet<String>) -> Result<bool, String> {
        let meta = fs::metadata(src).map_err(|e| format!("stat {src}: {e}"))?;
        if meta.is_dir() || !is_text(src)? {
            return Ok(true);
        }
        let content = fs::read_to_string(src).map_err(|e| format!("read {src}: {e}"))?;
        for s in siblings {
            if s != src && !self.map.contains_key(s) && content.contains(s.as_str()) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Stage a source under its clean name (rewriting text files) and add it.
    fn add_source(&mut self, src: &str) -> Result<String, String> {
        let meta = fs::metadata(src).map_err(|e| format!("stat {src}: {e}"))?;
        if meta.is_dir() {
            // Directories are added verbatim; rewriting their contents is out of
            // scope (Guix build-side modules rarely embed store paths).
            return nixstore::add_source(src);
        }
        let name = store_path_name(src);
        self.counter += 1;
        let dir = self.stage.join(self.counter.to_string());
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let staged = dir.join(name);

        if is_text(src)? {
            let content = fs::read_to_string(src).map_err(|e| format!("read {src}: {e}"))?;
            let rewritten = self.rewrite_str(&content);
            if FULL_STORE_PATH.is_match(&rewritten) {
                self.log(&format!(
                    "  WARNING: source {} still references a /gnu/store path after rewrite",
                    store_path_name(src)
                ));
            }
            fs::write(&staged, rewritten).map_err(|e| e.to_string())?;
        } else {
            fs::copy(src, &staged).map_err(|e| format!("copy {src}: {e}"))?;
        }
        nixstore::add_source(staged.to_str().unwrap())
    }

    /// Replace Guix store references in `s` with their Nix counterparts:
    /// full paths via the guix→nix map, and the bare store-directory constant
    /// (`/gnu/store` with no hash following) wholesale to `/nix/store`. Any
    /// full `/gnu/store/<hash>-` path left over is a genuine missing mapping.
    fn rewrite_str(&self, s: &str) -> String {
        if !s.contains("/gnu/store") {
            return s.to_string();
        }
        let mut out = s.to_string();
        for (guix, nix) in &self.map {
            if out.contains(guix.as_str()) {
                out = out.replace(guix.as_str(), nix);
            }
        }
        BARE_STORE_DIR
            .replace_all(&out, "/nix/store$1")
            .into_owned()
    }

    fn warn_leftover(&self, drv_path: &str, drv: &Derivation) {
        let mut hit = FULL_STORE_PATH.is_match(&drv.builder)
            || drv.args.iter().any(|a| FULL_STORE_PATH.is_match(a));
        for e in &drv.env {
            hit |= FULL_STORE_PATH.is_match(&e.value);
        }
        if hit {
            self.log(&format!(
                "  WARNING: {} still references a /gnu/store path after rewrite (missing mapping?)",
                store_path_name(drv_path)
            ));
        }
    }
}

/// Heuristic: a file is text if its first 1 KiB contains no NUL byte.
fn is_text(path: &str) -> Result<bool, String> {
    use std::io::Read;
    let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 1024];
    let n = f.read(&mut buf).map_err(|e| e.to_string())?;
    Ok(!buf[..n].contains(&0))
}

impl Drop for Splicer {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.stage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{EnvVar, Output};

    fn dl(url: &str, executable: bool) -> Derivation {
        let mut env = vec![
            EnvVar {
                key: "mirrors".into(),
                value: "/gnu/store/x-mirrors".into(),
            },
            EnvVar {
                key: "out".into(),
                value: "/gnu/store/x-foo.tar".into(),
            },
            EnvVar {
                key: "url".into(),
                value: url.into(),
            },
        ];
        if executable {
            env.push(EnvVar {
                key: "executable".into(),
                value: "1".into(),
            });
        }
        Derivation {
            outputs: vec![Output {
                name: "out".into(),
                path: "/gnu/store/x-foo.tar".into(),
                hash_algo: "sha256".into(),
                hash: "ab".into(),
            }],
            input_drvs: vec![],
            input_srcs: vec!["/gnu/store/x-mirrors".into()],
            system: "x86_64-linux".into(),
            builder: "builtin:download".into(),
            args: vec![],
            env,
        }
    }

    #[test]
    fn fetchurl_sets_builtin_and_drops_mirror_env() {
        let s = Splicer::new();
        let mut d = dl("(\"mirror://savannah/t/x.tar\")", false);
        s.to_fetchurl(&mut d, "https://chosen/x.tar".to_string());
        assert_eq!(d.builder, "builtin:fetchurl");
        assert_eq!(d.system, "builtin");
        assert!(d.input_srcs.is_empty());
        assert_eq!(d.env_get("url"), Some("https://chosen/x.tar"));
        assert!(d.env_get("mirrors").is_none());
    }

    #[test]
    fn fetchurl_keeps_executable() {
        let s = Splicer::new();
        let mut d = dl("\"https://real/bash\"", true);
        s.to_fetchurl(&mut d, "https://real/bash".to_string());
        assert_eq!(d.env_get("executable"), Some("1"));
    }

    #[test]
    fn upstream_mode_without_probing_takes_top_ranked() {
        let mut s = Splicer::new();
        s.upstream = true;
        s.probe = false;
        let d = dl(
            "(\"mirror://gnu/mes/m.tar.gz\" \"https://lilypond.org/janneke/m.tar.gz\")",
            false,
        );
        assert_eq!(
            s.choose_download_url(&d).unwrap(),
            "https://ftp.gnu.org/gnu/mes/m.tar.gz"
        );
    }

    #[test]
    fn default_mode_uses_guix_ca_mirror() {
        let mut s = Splicer::new();
        // The mirror keys on the URL basename (`tar`), not the store-path name.
        let mut d = dl("(\"https://example/bootstrap/tar\")", false);
        d.outputs[0].hash =
            "ba621bff6adc2e9e381f5907e0e86ad22b191678404e1f2888a5a924fa02031d".into();
        d.outputs[0].path = "/gnu/store/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-tar".into();
        assert_eq!(
            s.choose_download_url(&d).unwrap(),
            "https://bordeaux.guix.gnu.org/file/tar/sha256/07830bx29ad5i0l1ykj0g0b1jayjdblf01sr3ww9wbnwdbzinqms"
        );
    }

    #[test]
    fn ca_mirror_keys_on_url_basename_not_store_name() {
        // Regression: when the FOD output is named `hello-source` but the URL is
        // `.../hello-2.12.tar.gz`, the mirror must use the tarball basename.
        let mut s = Splicer::new();
        let mut d = dl(
            "(\"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz\")",
            false,
        );
        d.outputs[0].hash =
            "ba621bff6adc2e9e381f5907e0e86ad22b191678404e1f2888a5a924fa02031d".into();
        d.outputs[0].path = "/gnu/store/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-hello-source".into();
        assert_eq!(
            s.choose_download_url(&d).unwrap(),
            "https://bordeaux.guix.gnu.org/file/hello-2.12.tar.gz/sha256/07830bx29ad5i0l1ykj0g0b1jayjdblf01sr3ww9wbnwdbzinqms"
        );
    }

    #[test]
    fn download_file_name_from_url_basename() {
        let d = dl(
            "(\"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz?x=1\")",
            false,
        );
        assert_eq!(
            Splicer::download_file_name(&d).as_deref(),
            Some("hello-2.12.tar.gz")
        );
        // Mirror URLs keep their last path segment as the basename.
        let m = dl("(\"mirror://gnu/hello/hello-2.12.tar.gz\")", false);
        assert_eq!(
            Splicer::download_file_name(&m).as_deref(),
            Some("hello-2.12.tar.gz")
        );
    }

    #[test]
    fn rewrite_str_maps_known_paths_only() {
        let mut s = Splicer::new();
        s.map
            .insert("/gnu/store/aaa-dep".into(), "/nix/store/bbb-dep".into());
        assert_eq!(
            s.rewrite_str("PATH=/gnu/store/aaa-dep/bin"),
            "PATH=/nix/store/bbb-dep/bin"
        );
        // Unknown path left intact (surfaces as a real build error later).
        assert_eq!(
            s.rewrite_str("/gnu/store/zzz-other"),
            "/gnu/store/zzz-other"
        );
    }
}
