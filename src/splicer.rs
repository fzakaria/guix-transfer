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

use crate::ast::{Derivation, InputDrv, store_path_name};
use crate::emit_nix::TranslatedDrv;
use crate::graph::DerivationGraph;
use crate::{hash, json, mirrors, net, nixstore};
use dashmap::DashMap;
use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Derivation env attributes that hold reference specifiers (whitespace-separated
/// store paths or output names) which the daemon validates against the build
/// outputs. Specifiers with no Nix translation must be filtered out of these.
const REFERENCE_CHECK_KEYS: &[&str] = &[
    "allowedReferences",
    "disallowedReferences",
    "allowedRequisites",
    "disallowedRequisites",
];

/// Disable the gnu-build-system `check` phase in a builder script by flipping the
/// `#:tests?` keyword argument off. Guix lowers `#:tests? #t` literally into the
/// builder gexp, so a string substitution is sufficient and robust.
fn disable_builder_tests(builder: &str) -> String {
    builder.replace("#:tests? #t", "#:tests? #f")
}

/// Keep only reference specifiers that survive translation. A specifier still
/// pointing at `/gnu/store` had no Nix mapping and is not a valid Nix reference
/// (Nix wants a /nix/store path or an output name), so it is dropped.
fn filter_reference_specifiers(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|tok| !tok.contains("/gnu/store"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Discover the inputDrvs that `builtins.derivation` would track from the string
/// context of `all_text` (concatenated builder/args/env). Returns, per already
/// translated derivation, the output names whose Nix store path appears in the
/// text. This mirrors how emit_nix's `builtins.derivation` infers dependencies,
/// so that `nix derivation add` (json.rs) and emit_nix agree — see the call site
/// in [`Splicer::translate_one`].
fn referenced_input_drvs(all_text: &str, translated: &[TranslatedDrv]) -> Vec<InputDrv> {
    let mut found: HashMap<String, Vec<String>> = HashMap::new();
    for t in translated {
        for (out_name, out_path) in &t.nix_outputs {
            if all_text.contains(out_path.as_str()) {
                found
                    .entry(t.nix_drv_path.clone())
                    .or_default()
                    .push(out_name.clone());
            }
        }
    }
    found
        .into_iter()
        .map(|(path, mut outputs)| {
            outputs.sort();
            outputs.dedup();
            InputDrv { path, outputs }
        })
        .collect()
}

/// Merge `additions` into `existing` inputDrvs: union output sets per drv path,
/// adding new entries as needed. Output lists are left sorted and deduped.
fn merge_input_drvs(existing: &mut Vec<InputDrv>, additions: Vec<InputDrv>) {
    for add in additions {
        match existing.iter_mut().find(|i| i.path == add.path) {
            Some(e) => e.outputs.extend(add.outputs),
            None => existing.push(add),
        }
    }
    for i in existing {
        i.outputs.sort();
        i.outputs.dedup();
    }
}

/// A `builtin:git-download` source translated to a `pkgs.fetchgit` fixed-output
/// derivation. Nix has no `git-download` daemon builder; `pkgs.fetchgit` is a
/// build-time FOD whose store path is fixed by its hash, so it registers without
/// fetching (the daemon clones lazily at build time) and reproduces Guix's
/// git-fetch tree exactly — verified across tags, full/short SHAs, export-ignore
/// repos, and submodules.
#[derive(Clone, Debug)]
pub struct GitSource {
    pub url: String,
    /// Guix's `commit` (tag, full or short SHA) — passed straight to fetchgit,
    /// which resolves it at build time.
    pub rev: String,
    /// Store-object name, e.g. `guile-png-0.8.0-checkout`.
    pub name: String,
    /// Guix's recursive sha256, as an SRI string (fetchgit `hash`).
    pub hash_sri: String,
    /// Whether to fetch submodules (Guix `recursive?`).
    pub submodules: bool,
    /// The fetchgit derivation's `.drv` path (what consumers reference).
    pub drv_path: String,
    /// The fetchgit output path (= Guix's checkout path, recomputed).
    pub out_path: String,
}

/// Strip the surrounding quotes Guix adds via `object->string` to the `url` env
/// var of a `builtin:git-download` derivation (`"https://…"` → `https://…`).
fn unquote_guix_string(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

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
    pub map: DashMap<String, String>,
    /// Staging directory for rewritten sources before `nix-store --add`.
    stage: std::path::PathBuf,
    counter: AtomicUsize,
    /// Memoised URL reachability probes (`url → ok`, upstream mode only).
    url_cache: DashMap<String, bool>,
    pub verbose: bool,
    /// Fetch download seeds from their original upstream mirrors (with probing)
    /// instead of the Guix content-addressed mirror.
    pub upstream: bool,
    /// In upstream mode, probe candidate URLs before committing to one.
    pub probe: bool,
    /// Rewrite `#:tests? #t` → `#:tests? #f` in `*-builder` scripts so the
    /// gnu-build-system `check` phase is skipped. Done at translation time so the
    /// change is baked into the hashed builder and stays consistent with every
    /// downstream reference.
    pub disable_tests: bool,
    /// The Nix store directory (e.g. `/nix/store`), detected from the first
    /// derivation added.  Used to rewrite bare `/gnu/store` references.
    nix_store_dir: Mutex<Option<String>>,
    /// Translated derivations collected for `--emit-nix`.
    pub translated: Mutex<Vec<TranslatedDrv>>,
    /// `builtin:git-download` sources, keyed by their *Guix* `.drv` path, with
    /// the data emit_nix needs to render a `pkgs.fetchgit` call.
    pub git_sources: DashMap<String, GitSource>,
    /// nixpkgs git revision whose `fetchgit` translates `builtin:git-download`.
    /// Reached via `builtins.getFlake` — the only pinned form that imports in
    /// pure flake eval (`import <store-path>` is forbidden there). The sync
    /// passes its own nixpkgs rev; this is a fallback default.
    pub nixpkgs_rev: String,
    progress_counter: AtomicUsize,
}

/// nixpkgs flake URL for a pinned rev, reachable in pure evaluation mode.
pub fn nixpkgs_flake_ref(rev: &str) -> String {
    format!("github:NixOS/nixpkgs/{rev}")
}

/// A recent nixpkgs-unstable rev used when `--nixpkgs` is not given. Any rev
/// with `fetchgit` works (the fetched tree is hash-pinned to Guix's).
const DEFAULT_NIXPKGS_REV: &str = "3e41b24abd260e8f71dbe2f5737d24122f972158";

impl Splicer {
    pub fn new() -> Self {
        let stage = std::env::temp_dir().join(format!("guix-transfer-{}", std::process::id()));
        Self {
            map: DashMap::new(),
            stage,
            counter: AtomicUsize::new(0),
            url_cache: DashMap::new(),
            verbose: false,
            upstream: false,
            probe: true,
            disable_tests: false,
            nix_store_dir: Mutex::new(None),
            translated: Mutex::new(Vec::new()),
            git_sources: DashMap::new(),
            nixpkgs_rev: DEFAULT_NIXPKGS_REV.to_string(),
            progress_counter: AtomicUsize::new(0),
        }
    }

    /// Translate the whole graph; returns the final (root) Nix `.drv` path.
    pub fn run(&self, graph: &DerivationGraph) -> Result<String, String> {
        fs::create_dir_all(&self.stage)
            .map_err(|e| format!("create stage dir {}: {e}", self.stage.display()))?;
        let total = graph.order.len();
        let mut last = String::new();

        let layers = graph.compute_layers();
        for layer in layers {
            let results: Result<Vec<String>, String> = layer
                .par_iter()
                .map(|drv_path| {
                    let c = self.progress_counter.fetch_add(1, Ordering::SeqCst);
                    self.progress(c + 1, total, store_path_name(drv_path));
                    self.translate_one(drv_path, &graph.derivations[drv_path])
                })
                .collect();
            let mut paths = results?;
            if let Some(p) = paths.pop() {
                last = p;
            }
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

    fn translate_one(&self, guix_drv_path: &str, original: &Derivation) -> Result<String, String> {
        // A git checkout has no Nix daemon builder; translate it to a
        // pkgs.fetchgit FOD (see `translate_git_download`).
        if original.builder == "builtin:git-download" {
            return self.translate_git_download(guix_drv_path, original);
        }

        let mut drv = original.clone();

        if drv.builder == "builtin:download" {
            let url = self.choose_download_url(&drv)?;
            self.to_fetchurl(&mut drv, url);
        } else {
            self.add_sources(&mut drv)?;
        }

        // Rewrite all known store paths in inputs, builder, args, env. A
        // `builtin:git-download` input maps to its fetchgit `.drv`, so it stays a
        // normal inputDrv (like a `builtin:download` FOD).
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
                if REFERENCE_CHECK_KEYS.contains(&e.key.as_str()) {
                    // Reference-check attributes hold a whitespace-separated list
                    // of reference specifiers. Drop any that still point at
                    // /gnu/store: those have no Nix translation (e.g. a bootstrap
                    // input that is *disallowed*, hence never a build input), and
                    // Nix rejects such specifiers — it expects a /nix/store path
                    // or an output name.
                    e.value = filter_reference_specifiers(&e.value);
                }
            }
        }
        // Drop reference-check attributes left empty after filtering, so we don't
        // emit a vacuous `disallowedReferences = ""` (which Nix would treat as an
        // empty allow-list rather than "no constraint").
        drv.env
            .retain(|e| !(REFERENCE_CHECK_KEYS.contains(&e.key.as_str()) && e.value.is_empty()));

        // Nix's `builtins.derivation` injects `name`, `system`, `builder`
        // into env unconditionally (primops.cc line 1692).  Guix derivations
        // don't include these, so we add them here so `nix derivation add`
        // produces the same hash as a `builtins.derivation` Nix expression.
        let drv_name = crate::ast::derivation_name(guix_drv_path).to_string();
        for (key, value) in [
            ("name", drv_name.as_str()),
            ("system", drv.system.as_str()),
            ("builder", drv.builder.as_str()),
        ] {
            if !drv.env.iter().any(|e| e.key == key) {
                drv.env.push(crate::ast::EnvVar {
                    key: key.to_string(),
                    value: value.to_string(),
                });
            }
        }

        // `builtins.derivation` only tracks dependencies via string context in
        // attribute values. If an input drv output is only referenced inside an
        // inputSrc file (e.g. a build script), the .nix expression won't see
        // it. Collect such "phantom" deps and add them to a __phantom_deps env
        // var so both `nix derivation add` and `builtins.derivation` agree.
        if drv.builder != "builtin:fetchurl" {
            let all_text: String = {
                let mut s = drv.builder.clone();
                for a in &drv.args {
                    s.push(' ');
                    s.push_str(a);
                }
                for e in &drv.env {
                    s.push(' ');
                    s.push_str(&e.value);
                }
                s
            };
            let mut phantom = Vec::new();
            for input in &original.input_drvs {
                for out_name in &input.outputs {
                    let translated_lock = self.translated.lock().unwrap();
                    let mut nix_out_path = translated_lock
                        .iter()
                        .find(|t| t.guix_drv_path == input.path)
                        .and_then(|t| t.nix_outputs.get(out_name).cloned());
                    drop(translated_lock);

                    if nix_out_path.is_none()
                        && let Some(mapped_drv) = self.map.get(&input.path)
                    {
                        nix_out_path = nixstore::output_path_of(mapped_drv.value(), out_name);
                        if let Some(p) = &nix_out_path
                            && !p.starts_with('/')
                        {
                            let store_dir = mapped_drv
                                .value()
                                .rsplit_once('/')
                                .map(|(d, _)| d)
                                .unwrap_or("/nix/store");
                            nix_out_path = Some(format!("{store_dir}/{p}"));
                        }
                    }

                    if let Some(out_path) = nix_out_path
                        && !all_text.contains(&out_path)
                    {
                        phantom.push(out_path);
                    }
                }
            }
            if !phantom.is_empty() {
                phantom.sort();
                drv.env.push(crate::ast::EnvVar {
                    key: "__phantom_deps".to_string(),
                    value: phantom.join(" "),
                });
            }
        }

        if !drv.input_srcs.is_empty() {
            drv.env.push(crate::ast::EnvVar {
                key: "srcs".to_string(),
                value: drv.input_srcs.join(" "),
            });
        }

        // Align inputDrvs with `builtins.derivation`'s string-context tracking.
        //
        // emit_nix emits each drv as a `builtins.derivation`, which derives its
        // inputDrvs from the string context of EVERY attribute value — so a store
        // path appearing only in an env var (e.g. `allowedReferences` naming
        // `gcc-cross-boot0:lib`, or `__phantom_deps`) becomes an inputDrv. But
        // `nix derivation add` (json.rs) takes inputDrvs solely from the explicit
        // list, which Guix populates from *build* edges — and a reference-check
        // constraint like `allowedReferences` is not a build edge. The two then
        // disagree on a multi-output dep's output set, producing different .drv
        // paths (the "split-brain" bug: consumers bake the json path, Nix builds
        // the emit path -> `ld: cannot find crt1.o`).
        //
        // Fix: ensure input_drvs contains every translated output referenced
        // anywhere in builder/args/env, exactly as builtins.derivation would.
        {
            let mut all_text = drv.builder.clone();
            for a in &drv.args {
                all_text.push(' ');
                all_text.push_str(a);
            }
            for e in &drv.env {
                all_text.push(' ');
                all_text.push_str(&e.value);
            }
            // Only prior drvs are translated (bottom-up), so this never matches
            // our own (still-blank) outputs.
            let translated = self.translated.lock().unwrap();
            merge_input_drvs(
                &mut drv.input_drvs,
                referenced_input_drvs(&all_text, &translated),
            );
            drop(translated);
        }

        // Blank our own output paths (Nix recomputes input-addressed ones;
        // fixed-output ones are derived from the hash).
        for o in &mut drv.outputs {
            o.path = String::new();
        }

        self.warn_leftover(guix_drv_path, &drv);

        let value = json::to_nix_json(&drv, guix_drv_path)?;
        let nix_drv = nixstore::derivation_add(&value)?;
        self.log(&format!(
            "  {} -> {}",
            store_path_name(guix_drv_path),
            nix_drv
        ));

        // Map the drv path and every output path for parents. `nix derivation
        // show` reports output paths store-relative; re-prefix with the store
        // dir taken from the (full) drv path so downstream string rewrites work.
        self.map.insert(guix_drv_path.to_string(), nix_drv.clone());
        // Initialise the global Nix store prefix if we haven't already.
        let nix_outputs = nixstore::output_paths(&nix_drv)?;
        let store_dir = nix_drv
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or("/nix/store");
        if self.nix_store_dir.lock().unwrap().is_none() {
            *self.nix_store_dir.lock().unwrap() = Some(store_dir.to_string());
        }

        // Collect full output paths for emit-nix.
        let mut full_outputs = std::collections::HashMap::new();
        let store_dir = self
            .nix_store_dir
            .lock()
            .unwrap()
            .clone()
            .unwrap_or("/nix/store".to_string());
        for out in &original.outputs {
            if let Some(nix_out) = nix_outputs.get(&out.name) {
                let full = if nix_out.starts_with('/') {
                    nix_out.clone()
                } else {
                    format!("{store_dir}/{nix_out}")
                };
                self.map.insert(out.path.clone(), full.clone());
                full_outputs.insert(out.name.clone(), full);
            }
        }

        self.translated.lock().unwrap().push(TranslatedDrv {
            guix_drv_path: guix_drv_path.to_string(),
            nix_drv_path: nix_drv.clone(),
            drv,
            nix_outputs: full_outputs,
        });

        Ok(nix_drv)
    }

    /// Translate a `builtin:git-download` derivation into a `pkgs.fetchgit`
    /// fixed-output derivation. fetchgit is a build-time FOD, so it is registered
    /// from its hash alone (no fetching during translation; the daemon clones
    /// lazily at build time) and reproduces Guix's git-fetch tree exactly.
    ///
    /// We instantiate the fetchgit derivation with a cheap `nix eval` to learn
    /// its `.drv` and output paths, record a [`GitSource`] for emit_nix, and map
    /// the Guix drv + output paths onto them. The checkout is then a normal
    /// inputDrv to consumers, exactly like a `builtin:download` FOD.
    fn translate_git_download(
        &self,
        guix_drv_path: &str,
        original: &Derivation,
    ) -> Result<String, String> {
        let out = original
            .outputs
            .first()
            .ok_or("git-download: derivation has no output")?;
        let name = original
            .env_get("name")
            .map(str::to_string)
            .unwrap_or_else(|| store_path_name(&out.path).to_string());
        let url = unquote_guix_string(original.env_get("url").unwrap_or(""));
        let rev = original.env_get("commit").unwrap_or("").to_string();
        let submodules = original.env_get("recursive?") == Some("#t");
        if url.is_empty() || rev.is_empty() {
            return Err(format!("git-download {name}: missing url/commit"));
        }
        let hash_sri = hash::guix_to_nix(&out.hash_algo, &out.hash, false)
            .map_err(|e| format!("git-download {name}: bad hash: {e}"))?
            .sri;

        let (drv_path, out_path) = self.fetchgit_paths(&url, &rev, &hash_sri, submodules, &name)?;

        self.git_sources.insert(
            guix_drv_path.to_string(),
            GitSource {
                url,
                rev,
                name,
                hash_sri,
                submodules,
                drv_path: drv_path.clone(),
                out_path: out_path.clone(),
            },
        );
        // The Guix drv maps to the fetchgit drv; the Guix output to its output.
        self.map.insert(guix_drv_path.to_string(), drv_path.clone());
        self.map.insert(out.path.clone(), out_path);
        Ok(drv_path)
    }

    /// Instantiate `pkgs.fetchgit { … }` (no build/fetch) and return its
    /// `(drv_path, out_path)`. The output path is fixed by the hash, so this is
    /// a pure path computation that just writes the `.drv` to the store.
    fn fetchgit_paths(
        &self,
        url: &str,
        rev: &str,
        hash_sri: &str,
        submodules: bool,
        name: &str,
    ) -> Result<(String, String), String> {
        fn nix_lit(s: &str) -> String {
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
        }
        let expr = format!(
            "let g = (builtins.getFlake {flake}).legacyPackages.x86_64-linux.fetchgit {{ \
               url = {url}; rev = {rev}; hash = {hash}; name = {name}; \
               fetchSubmodules = {sub}; }}; \
             in {{ drv = g.drvPath; out = g.outPath; }}",
            flake = nix_lit(&nixpkgs_flake_ref(&self.nixpkgs_rev)),
            url = nix_lit(url),
            rev = nix_lit(rev),
            hash = nix_lit(hash_sri),
            name = nix_lit(name),
            sub = if submodules { "true" } else { "false" },
        );
        let output = std::process::Command::new("nix")
            .args(["eval", "--impure", "--json", "--expr", &expr])
            .output()
            .map_err(|e| format!("git-download {name}: running nix eval: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "git-download {name}: instantiating pkgs.fetchgit failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let v: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("git-download {name}: parse fetchgit output: {e}"))?;
        let drv = v["drv"]
            .as_str()
            .ok_or_else(|| format!("git-download {name}: no drvPath"))?
            .to_string();
        let out = v["out"]
            .as_str()
            .ok_or_else(|| format!("git-download {name}: no outPath"))?
            .to_string();
        Ok((drv, out))
    }

    /// Choose a single URL for a `builtin:download` derivation.
    fn choose_download_url(&self, drv: &Derivation) -> Result<String, String> {
        let is_executable = drv.env_get("executable") == Some("1");
        let mut candidates = Vec::new();
        if !self.upstream
            && !is_executable
            && let Some(out) = drv.outputs.first().filter(|o| !o.hash.is_empty())
        {
            // The Guix content-addressed mirror is keyed by the OUTPUT store
            // name (e.g. `guile-zlib-0.2.2.tar.gz`), NOT the source URL's
            // basename. For a GitHub tag archive the URL basename is
            // `v0.2.2.tar.gz`, which 404s on the mirror; the output store name
            // carries the real package name. (See NOTES.md "URL selection".)
            let name = store_path_name(&out.path);
            if let Ok(b_url) = hash::guix_ca_mirror_url(name, &out.hash) {
                candidates.push(b_url);
            }
        }
        let raw_url = drv.env_get("url").unwrap_or("").to_string();
        candidates.extend(mirrors::candidate_urls(&mirrors::extract_urls(&raw_url)));
        if candidates.is_empty() {
            return Err(format!("no usable URL in download env {raw_url:?}"));
        }
        if !self.probe {
            return Ok(candidates[0].clone());
        }
        if let Some(found) = candidates.par_iter().find_any(|url| {
            if let Some(ok) = self.url_cache.get(*url) {
                return *ok.value();
            }
            let ok = net::url_ok(url);
            self.url_cache.insert((*url).clone(), ok);
            ok
        }) {
            return Ok(found.clone());
        }
        self.log(&format!(
            "    WARNING: none reachable, using {}",
            candidates[0]
        ));
        Ok(candidates[0].clone())
    }

    fn to_fetchurl(&self, drv: &mut Derivation, url: String) {
        let executable = drv.env_get("executable") == Some("1");
        drv.builder = "builtin:fetchurl".to_string();
        drv.system = "builtin".to_string();
        drv.args.clear();
        drv.input_srcs.clear();
        drv.input_drvs.clear();
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
        if let Some(out) = drv.outputs.first() {
            env.push(crate::ast::EnvVar {
                key: out.name.clone(),
                value: String::new(),
            });
        }
        env.retain(|e| !DROP_DOWNLOAD_ENV.contains(&e.key.as_str()));
        drv.env = env;
    }

    fn add_sources(&self, drv: &mut Derivation) -> Result<(), String> {
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
            let mut staged_paths = Vec::new();
            let mut src_list = Vec::new();

            for src in std::mem::take(&mut pending) {
                if self.src_ready(&src, &siblings)? {
                    staged_paths.push(self.stage_source(&src)?);
                    src_list.push(src);
                } else {
                    still.push(src);
                }
            }
            if !staged_paths.is_empty() {
                let nix_paths = nixstore::add_sources(&staged_paths)?;
                for (src, nix) in src_list.into_iter().zip(nix_paths) {
                    self.map.insert(src, nix);
                }
                progressed = true;
            }
            pending = still;
            if !progressed {
                let mut staged_paths = Vec::new();
                for src in std::mem::take(&mut pending) {
                    staged_paths.push(self.stage_source(&src)?);
                }
                if !staged_paths.is_empty() {
                    let nix_paths = nixstore::add_sources(&staged_paths)?;
                    for (src, nix) in pending.into_iter().zip(nix_paths) {
                        self.map.insert(src, nix);
                    }
                }
                break;
            }
        }

        drv.input_srcs = srcs
            .iter()
            .map(|s| {
                self.map
                    .get(s)
                    .map(|r| r.value().clone())
                    .unwrap_or_else(|| s.clone())
            })
            .collect();
        Ok(())
    }

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

    fn stage_source(&self, src: &str) -> Result<String, String> {
        let meta = fs::metadata(src).map_err(|e| format!("stat {src}: {e}"))?;
        if meta.is_dir() {
            return Ok(src.to_string());
        }
        let name = store_path_name(src);
        let c = self.counter.fetch_add(1, Ordering::SeqCst);
        let dir = self.stage.join(c.to_string());
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let staged = dir.join(name);
        if is_text(src)? {
            let content = fs::read_to_string(src).map_err(|e| format!("read {src}: {e}"))?;
            let mut rewritten = self.rewrite_str(&content);
            if self.disable_tests && name.ends_with("-builder") {
                rewritten = disable_builder_tests(&rewritten);
            }
            fs::write(&staged, rewritten).map_err(|e| e.to_string())?;
        } else {
            fs::copy(src, &staged).map_err(|e| format!("copy {src}: {e}"))?;
        }
        Ok(staged.to_str().unwrap().to_string())
    }

    fn rewrite_str(&self, s: &str) -> String {
        if !s.contains("/gnu/store") {
            return s.to_string();
        }
        let mut out = s.to_string();
        for guix in self.map.iter() {
            if out.contains(guix.key().as_str()) {
                out = out.replace(guix.key().as_str(), guix.value());
            }
        }
        if let Some(dir) = &*self.nix_store_dir.lock().unwrap() {
            let replacement = format!("{dir}$1");
            BARE_STORE_DIR
                .replace_all(&out, replacement.as_str())
                .into_owned()
        } else {
            out
        }
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
    fn disable_builder_tests_flips_tests_flag() {
        assert_eq!(
            disable_builder_tests("(gnu-build #:source \"x\" #:tests? #t #:test-target \"check\")"),
            "(gnu-build #:source \"x\" #:tests? #f #:test-target \"check\")"
        );
        // Idempotent / no-op when already disabled or absent.
        assert_eq!(disable_builder_tests("#:tests? #f"), "#:tests? #f");
        assert_eq!(disable_builder_tests("no flag here"), "no flag here");
    }

    #[test]
    fn filter_reference_specifiers_drops_untranslated_and_keeps_rest() {
        // Mixed: a translated /nix/store path and an output name survive; the
        // untranslated /gnu/store bootstrap path is dropped.
        assert_eq!(
            filter_reference_specifiers(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc out /gnu/store/zb0sq4hj0aw5qk0p8n91vv19fc0fild8-binutils-bootstrap-0"
            ),
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc out"
        );
        // All untranslated → empty (the caller then drops the attribute).
        assert_eq!(
            filter_reference_specifiers(
                "/gnu/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-binutils-bootstrap-0"
            ),
            ""
        );
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
        s.probe = false;
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
    fn ca_mirror_keys_on_output_store_name() {
        // The CA mirror is keyed by the OUTPUT store name, NOT the source URL's
        // basename. A GitHub tag archive proves it in the wild: the URL basename
        // `v0.2.2.tar.gz` 404s on bordeaux while the output store name
        // `guile-zlib-0.2.2.tar.gz` 200s. So when the output is named
        // `hello-source` but the URL ends in `hello-2.12.tar.gz`, the mirror URL
        // uses `hello-source`.
        let mut s = Splicer::new();
        s.probe = false;
        let mut d = dl(
            "(\"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz\")",
            false,
        );
        d.outputs[0].hash =
            "ba621bff6adc2e9e381f5907e0e86ad22b191678404e1f2888a5a924fa02031d".into();
        d.outputs[0].path = "/gnu/store/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-hello-source".into();
        assert_eq!(
            s.choose_download_url(&d).unwrap(),
            "https://bordeaux.guix.gnu.org/file/hello-source/sha256/07830bx29ad5i0l1ykj0g0b1jayjdblf01sr3ww9wbnwdbzinqms"
        );
    }

    #[test]
    fn rewrite_str_maps_known_paths_only() {
        let s = Splicer::new();
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

    fn translated(nix_drv: &str, outs: &[(&str, &str)]) -> TranslatedDrv {
        TranslatedDrv {
            guix_drv_path: String::new(),
            nix_drv_path: nix_drv.into(),
            drv: Derivation {
                outputs: vec![],
                input_drvs: vec![],
                input_srcs: vec![],
                system: String::new(),
                builder: String::new(),
                args: vec![],
                env: vec![],
            },
            nix_outputs: outs
                .iter()
                .map(|(n, p)| (n.to_string(), p.to_string()))
                .collect(),
        }
    }

    // Regression for the "split-brain" bug: an output referenced only in an env
    // var like `allowedReferences` (e.g. glibc -> gcc-cross-boot0:lib) is tracked
    // as an inputDrv by `builtins.derivation` (emit_nix) but missed by the
    // explicit `nix derivation add` list (json.rs). referenced_input_drvs must
    // recover it so the two serializers agree.
    #[test]
    fn referenced_input_drvs_finds_outputs_in_text() {
        let t = vec![translated(
            "/nix/store/dep.drv",
            &[
                ("out", "/nix/store/aaa-dep"),
                ("lib", "/nix/store/bbb-dep-lib"),
            ],
        )];
        // Only the `lib` output appears (as it would inside allowedReferences).
        let text = "allowedReferences=/nix/store/bbb-dep-lib out";
        let got = referenced_input_drvs(text, &t);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "/nix/store/dep.drv");
        assert_eq!(got[0].outputs, vec!["lib".to_string()]);
    }

    #[test]
    fn merge_input_drvs_unions_outputs() {
        // Existing build edge declares only `out`; allowedReferences adds `lib`.
        let mut existing = vec![InputDrv {
            path: "/nix/store/dep.drv".into(),
            outputs: vec!["out".into()],
        }];
        merge_input_drvs(
            &mut existing,
            vec![InputDrv {
                path: "/nix/store/dep.drv".into(),
                outputs: vec!["lib".into()],
            }],
        );
        assert_eq!(existing.len(), 1);
        assert_eq!(
            existing[0].outputs,
            vec!["lib".to_string(), "out".to_string()]
        );
    }

    #[test]
    fn unquote_guix_string_strips_object_to_string_quotes() {
        // Guix's git-download `url` env is `(object->string url)`, i.e. quoted.
        assert_eq!(
            unquote_guix_string("\"https://github.com/wolfcw/libfaketime\""),
            "https://github.com/wolfcw/libfaketime"
        );
        assert_eq!(unquote_guix_string("https://x/y"), "https://x/y"); // already bare
        assert_eq!(unquote_guix_string("  \"a\"  "), "a"); // trims surrounding ws
    }
}
