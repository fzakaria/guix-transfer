//! Bottom-up translation of a Guix derivation graph into Nix derivations.
//!
//! For each derivation, in dependency order:
//!   1. `builtin:download` → pinned nixpkgs `fetchurl { urls = […]; }` FOD.
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
use crate::progress::{Mode, Progress};
use crate::{emit_nix, hash, json, mirrors, nixstore};
use dashmap::DashMap;
use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
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

/// Render Guix's untyped git `commit` as a revision fetchgit can resolve.
/// Guix accepts tags and commit IDs in the same field, while fetchgit needs a
/// tag's full ref when its name happens to look hexadecimal (for example,
/// iputils' `20250605`). Guix package definitions use full SHA-1 IDs, apart
/// from occasional abbreviated IDs containing hexadecimal letters.
fn fetchgit_revision(commit: &str) -> String {
    let is_hex = commit.bytes().all(|byte| byte.is_ascii_hexdigit());
    let is_full_sha1 = is_hex && commit.len() == 40;
    let is_abbreviated_sha1 = is_hex
        && (7..40).contains(&commit.len())
        && commit.bytes().any(|byte| byte.is_ascii_alphabetic());

    if is_full_sha1 || is_abbreviated_sha1 || commit.starts_with("refs/") {
        commit.to_string()
    } else {
        format!("refs/tags/{commit}")
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
    /// Guix's `commit`, normalized to a fetchgit revision or full tag ref.
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

/// How many sources one prefetch `nix eval` instantiates. Each evaluator
/// pays the nixpkgs-via-getFlake evaluation once (~0.7s of CPU) and then
/// instantiates its whole chunk for milliseconds apiece, so bigger chunks
/// amortize better; the cap only bounds expression size and evaluator
/// memory.
const PREFETCH_EVAL_CHUNK_SIZE: usize = 256;

/// One source in the batched prefetch: the Guix `.drv` path it translates,
/// and the Nix call (`fetchurl { … }` / `fetchgit { … }`) instantiating it.
struct PrefetchEntry {
    guix_drv_path: String,
    call: String,
}

/// The argument set for one git source, shared verbatim by the batched
/// prefetch and the per-source fallback in [`Splicer::fetchgit_paths`] so
/// both instantiate the identical fetchgit derivation.
fn fetchgit_args(gs: &GitSource) -> String {
    format!(
        "{{ url = {url}; rev = {rev}; hash = {hash}; name = {name}; fetchSubmodules = {sub}; }}",
        url = emit_nix::nix_str_literal(&gs.url),
        rev = emit_nix::nix_str_literal(&gs.rev),
        hash = emit_nix::nix_str_literal(&gs.hash_sri),
        name = emit_nix::nix_str_literal(&gs.name),
        sub = if gs.submodules { "true" } else { "false" },
    )
}

/// Render one prefetch chunk as a single eval expression: an attrset keyed
/// by Guix `.drv` path whose values carry each source derivation's
/// drvPath/outPath. The fetchurl/fetchgit helpers are bound once, so the
/// chunk shares one nixpkgs evaluation.
fn prefetch_expr(nixpkgs_rev: &str, entries: &[PrefetchEntry]) -> String {
    let mut expr = format!(
        "let fetchurl = {fetch}; fetchgit = (builtins.getFlake {flake}).legacyPackages.x86_64-linux.fetchgit; in {{",
        fetch = emit_nix::fetch_url_fn(nixpkgs_rev),
        flake = emit_nix::nix_str_literal(&nixpkgs_flake_ref(nixpkgs_rev)),
    );
    for entry in entries {
        expr.push_str(&format!(
            " {key} = let f = {call}; in {{ drv = f.drvPath; out = f.outPath; }};",
            key = emit_nix::nix_str_literal(&entry.guix_drv_path),
            call = entry.call,
        ));
    }
    expr.push_str(" }");
    expr
}

/// Build the [`UrlSource`] (candidate URLs, hash, flags; store paths left
/// empty) for a `builtin:download` derivation. Shared by the batched
/// prefetch and [`Splicer::translate_download`] so both instantiate the
/// identical fetchurl call.
fn url_source_for(original: &Derivation) -> Result<UrlSource, String> {
    let out = original
        .outputs
        .first()
        .ok_or("download: derivation has no output")?;
    // The Guix content-addressed mirror is keyed by the OUTPUT store name
    // (e.g. `guile-zlib-0.2.2.tar.gz`), NOT the source URL's basename. For
    // a GitHub tag archive the URL basename is `v0.2.2.tar.gz`, which 404s
    // on the mirror; the output store name carries the real package name.
    // (See NOTES.md "URL selection".)
    let name = store_path_name(&out.path).to_string();
    if original.outputs.len() != 1 || out.hash.is_empty() {
        return Err(format!(
            "download {name}: expected exactly one fixed output"
        ));
    }
    let executable = original.env_get("executable") == Some("1");
    let hash = hash::guix_to_nix(&out.hash_algo, &out.hash, executable)
        .map_err(|e| format!("download {name}: bad hash: {e}"))?;
    let recursive = hash.method == "nar";

    let raw_url = original.env_get("url").unwrap_or("");
    let urls = download_candidate_urls(&name, &out.hash, recursive, raw_url)?;

    Ok(UrlSource {
        urls,
        name,
        hash_sri: hash.sri,
        recursive,
        executable,
        drv_path: String::new(),
        out_path: String::new(),
    })
}

/// Build the [`GitSource`] (url, rev, hash, name, submodules; store paths
/// left empty) for a `builtin:git-download` derivation. Shared by the
/// batched prefetch and [`Splicer::translate_git_download`] so both
/// instantiate the identical fetchgit call.
fn git_source_for(original: &Derivation) -> Result<GitSource, String> {
    let out = original
        .outputs
        .first()
        .ok_or("git-download: derivation has no output")?;
    let name = original
        .env_get("name")
        .map(str::to_string)
        .unwrap_or_else(|| store_path_name(&out.path).to_string());
    let url = unquote_guix_string(original.env_get("url").unwrap_or(""));
    let commit = original.env_get("commit").unwrap_or("");
    let submodules = original.env_get("recursive?") == Some("#t");
    if url.is_empty() || commit.is_empty() {
        return Err(format!("git-download {name}: missing url/commit"));
    }
    let rev = fetchgit_revision(commit);
    let hash_sri = hash::guix_to_nix(&out.hash_algo, &out.hash, false)
        .map_err(|e| format!("git-download {name}: bad hash: {e}"))?
        .sri;
    Ok(GitSource {
        url,
        rev,
        name,
        hash_sri,
        submodules,
        drv_path: String::new(),
        out_path: String::new(),
    })
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

/// A `builtin:download` source translated to a `pkgs.fetchurl` fixed-output
/// derivation. Nix's `builtin:fetchurl` accepts exactly one URL and cannot
/// fall back across a mirror list, so instead the full candidate list is baked
/// into a pinned nixpkgs `fetchurl { urls = […]; }` FOD: the fallback happens
/// at build time, inside the derivation, and URL availability never changes
/// the derivation identity (no probing during translation).
#[derive(Clone, Debug)]
pub struct UrlSource {
    /// Ordered candidate URLs, best first (CA mirror, then upstream mirrors).
    pub urls: Vec<String>,
    /// Store-object name, e.g. `hello-2.12.tar.gz`.
    pub name: String,
    /// Guix's hash, as an SRI string (fetchurl `hash`).
    pub hash_sri: String,
    /// NAR (recursive) hash mode — Guix `r:` algos and executable downloads.
    pub recursive: bool,
    /// Whether the fetched file is marked executable (Guix `executable=1`).
    pub executable: bool,
    /// The fetchurl derivation's `.drv` path (what consumers reference).
    pub drv_path: String,
    /// The fetchurl output path (= Guix's source path, recomputed).
    pub out_path: String,
}

/// Ordered candidate URLs for a download, best first, all baked into the
/// fetchurl FOD. The Guix content-addressed mirror leads: it serves any source
/// Guix's CI has seen, keyed by the flat content hash we already have, so it
/// outlives flaky upstream mirrors. The upstream declarations follow as
/// build-time fallbacks. NAR-hashed outputs skip the CA mirror because its
/// lookup key is the flat file hash, which a NAR hash never matches.
fn download_candidate_urls(
    name: &str,
    hash_hex: &str,
    recursive: bool,
    raw_url: &str,
) -> Result<Vec<String>, String> {
    let mut urls = Vec::new();
    if !recursive {
        urls.push(hash::guix_ca_mirror_url(name, hash_hex)?);
    }
    urls.extend(mirrors::candidate_urls(&mirrors::extract_urls(raw_url)));
    urls.dedup();
    if urls.is_empty() {
        return Err(format!("download {name}: no usable URL in {raw_url:?}"));
    }
    Ok(urls)
}

pub struct Splicer {
    /// Any Guix store path (drv, output, or source) → its Nix counterpart.
    pub map: DashMap<String, String>,
    /// Staging directory for rewritten sources before `nix-store --add`.
    stage: std::path::PathBuf,
    counter: AtomicUsize,
    pub verbose: bool,
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
    /// `builtin:download` sources, keyed by their *Guix* `.drv` path, with the
    /// data emit_nix needs to render a `pkgs.fetchurl` call.
    pub url_sources: DashMap<String, UrlSource>,
    /// `builtin:git-download` sources, keyed by their *Guix* `.drv` path, with
    /// the data emit_nix needs to render a `pkgs.fetchgit` call.
    pub git_sources: DashMap<String, GitSource>,
    /// nixpkgs git revision whose `fetchgit` translates `builtin:git-download`.
    /// Reached via `builtins.getFlake` — the only pinned form that imports in
    /// pure flake eval (`import <store-path>` is forbidden there). The sync
    /// passes its own nixpkgs rev; this is a fallback default.
    pub nixpkgs_rev: String,
    /// Batched-prefetch results: Guix source `.drv` path → (Nix `.drv`
    /// path, Nix output path). Filled by [`Splicer::prefetch_sources`];
    /// misses fall back to a per-source `nix eval`.
    prefetched: DashMap<String, (String, String)>,
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
            verbose: false,
            disable_tests: false,
            nix_store_dir: Mutex::new(None),
            translated: Mutex::new(Vec::new()),
            url_sources: DashMap::new(),
            git_sources: DashMap::new(),
            nixpkgs_rev: DEFAULT_NIXPKGS_REV.to_string(),
            prefetched: DashMap::new(),
        }
    }

    /// How progress lines should render for this run's verbosity.
    pub fn progress_mode(&self) -> Mode {
        if self.verbose {
            Mode::Verbose
        } else {
            Mode::Auto
        }
    }

    /// Translate the whole graph; returns the final (root) Nix `.drv` path.
    pub fn run(&self, graph: &DerivationGraph) -> Result<String, String> {
        fs::create_dir_all(&self.stage)
            .map_err(|e| format!("create stage dir {}: {e}", self.stage.display()))?;
        let mut last = String::new();

        self.prefetch_sources(graph);

        let progress = Progress::new(self.progress_mode(), graph.order.len());
        let layers = graph.compute_layers();
        for layer in layers {
            let results: Result<Vec<String>, String> = layer
                .par_iter()
                .map(|drv_path| {
                    // The handle keeps this derivation's in-flight line live
                    // for the duration of the translation; dropping the
                    // handle counts the step done, on error paths included.
                    let _active = progress.start(store_path_name(drv_path));
                    self.translate_one(drv_path, &graph.derivations[drv_path])
                })
                .collect();
            let mut paths = results?;
            if let Some(p) = paths.pop() {
                last = p;
            }
        }
        progress.done();
        Ok(last)
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            eprintln!("{msg}");
        }
    }

    /// Instantiate every `builtin:download` / `builtin:git-download` source
    /// in the graph up front, batched into a few `nix eval` calls. A
    /// per-source eval pays a full nixpkgs evaluation (~0.7s of CPU), and a
    /// wide first layer runs hundreds of them concurrently against one
    /// daemon — the dominant cost of translating download-heavy graphs.
    /// One evaluator per chunk pays nixpkgs once and instantiates the whole
    /// chunk in milliseconds apiece.
    ///
    /// Failures are soft: a source that fails to parse or a chunk whose
    /// eval fails is simply absent from the cache, and translation falls
    /// back to the per-source eval, whose error names the offending source
    /// with full context.
    fn prefetch_sources(&self, graph: &DerivationGraph) {
        let mut entries: Vec<PrefetchEntry> = Vec::new();
        for (guix_drv_path, drv) in &graph.derivations {
            let call = match drv.builder.as_str() {
                "builtin:download" => match url_source_for(drv) {
                    Ok(source) => format!("fetchurl {}", emit_nix::url_source_args(&source)),
                    Err(_) => continue,
                },
                "builtin:git-download" => match git_source_for(drv) {
                    Ok(source) => format!("fetchgit {}", fetchgit_args(&source)),
                    Err(_) => continue,
                },
                _ => continue,
            };
            entries.push(PrefetchEntry {
                guix_drv_path: guix_drv_path.clone(),
                call,
            });
        }
        if entries.is_empty() {
            return;
        }

        // Deterministic chunking (map iteration order is arbitrary).
        entries.sort_by(|a, b| a.guix_drv_path.cmp(&b.guix_drv_path));

        eprintln!(
            "Instantiating {} download/git sources in {} batched nix eval(s) ...",
            entries.len(),
            entries.len().div_ceil(PREFETCH_EVAL_CHUNK_SIZE)
        );
        let began = std::time::Instant::now();
        entries
            .par_chunks(PREFETCH_EVAL_CHUNK_SIZE)
            .for_each(|chunk| {
                let expr = prefetch_expr(&self.nixpkgs_rev, chunk);
                let output = match std::process::Command::new("nix")
                    .args(["eval", "--impure", "--json", "--expr", &expr])
                    .output()
                {
                    Ok(output) => output,
                    Err(e) => {
                        eprintln!(
                            "WARNING: could not run batched source instantiation ({e}); \
                             falling back to per-source evals"
                        );
                        return;
                    }
                };
                if !output.status.success() {
                    eprintln!(
                        "WARNING: a batched source instantiation of {} sources failed; \
                         they fall back to per-source evals",
                        chunk.len()
                    );
                    return;
                }

                match serde_json::from_slice::<HashMap<String, HashMap<String, String>>>(
                    &output.stdout,
                ) {
                    Ok(paths) => {
                        for (guix_drv_path, v) in paths {
                            if let (Some(drv), Some(out)) = (v.get("drv"), v.get("out")) {
                                self.prefetched
                                    .insert(guix_drv_path, (drv.clone(), out.clone()));
                            }
                        }
                    }
                    Err(e) => eprintln!(
                        "WARNING: could not parse batched source instantiation output ({e}); \
                         falling back to per-source evals"
                    ),
                }
            });
        eprintln!(
            "Instantiated {} of {} sources in {:.1}s.",
            self.prefetched.len(),
            entries.len(),
            began.elapsed().as_secs_f32()
        );
    }

    fn translate_one(&self, guix_drv_path: &str, original: &Derivation) -> Result<String, String> {
        // Downloads and git checkouts have no Nix daemon builder; translate
        // them to pinned nixpkgs FOD helpers (see `translate_download` and
        // `translate_git_download`). Both are instantiated, never fetched,
        // during translation.
        if original.builder == "builtin:download" {
            return self.translate_download(guix_drv_path, original);
        }
        if original.builder == "builtin:git-download" {
            return self.translate_git_download(guix_drv_path, original);
        }

        let mut drv = original.clone();
        self.add_sources(&mut drv)?;

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
        {
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
        let mut source = git_source_for(original)?;
        let (drv_path, out_path) = self.fetchgit_paths(guix_drv_path, &source)?;
        source.drv_path = drv_path.clone();
        source.out_path = out_path.clone();

        self.git_sources.insert(guix_drv_path.to_string(), source);
        // The Guix drv maps to the fetchgit drv; the Guix output to its output.
        self.map.insert(guix_drv_path.to_string(), drv_path.clone());
        self.map.insert(out.path.clone(), out_path);
        Ok(drv_path)
    }

    /// Instantiate `pkgs.fetchgit { … }` (no build/fetch) and return its
    /// `(drv_path, out_path)`. The output path is fixed by the hash, so this is
    /// a pure path computation that just writes the `.drv` to the store.
    /// The batched prefetch has usually computed the paths already; a cache
    /// miss falls back to a per-source eval, whose error names the source.
    fn fetchgit_paths(
        &self,
        guix_drv_path: &str,
        source: &GitSource,
    ) -> Result<(String, String), String> {
        if let Some(paths) = self.prefetched.get(guix_drv_path) {
            return Ok(paths.value().clone());
        }

        let name = &source.name;
        let expr = format!(
            "let f = (builtins.getFlake {flake}).legacyPackages.x86_64-linux.fetchgit {args}; \
             in {{ drv = f.drvPath; out = f.outPath; }}",
            flake = emit_nix::nix_str_literal(&nixpkgs_flake_ref(&self.nixpkgs_rev)),
            args = fetchgit_args(source),
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

    /// Translate a `builtin:download` derivation into a `pkgs.fetchurl`
    /// fixed-output derivation carrying the full candidate URL list. fetchurl
    /// is a build-time FOD, so it is registered from its hash alone (no
    /// fetching during translation; the daemon downloads lazily at build time,
    /// trying each URL in order).
    ///
    /// We instantiate the fetchurl derivation with a cheap `nix eval` to learn
    /// its `.drv` and output paths, record a [`UrlSource`] for emit_nix, and
    /// map the Guix drv + output paths onto them. The source is then a normal
    /// inputDrv to consumers, exactly like the old `builtin:fetchurl` FOD.
    fn translate_download(
        &self,
        guix_drv_path: &str,
        original: &Derivation,
    ) -> Result<String, String> {
        let out = original
            .outputs
            .first()
            .ok_or("download: derivation has no output")?;
        let mut source = url_source_for(original)?;
        let (drv_path, out_path) = self.fetchurl_paths(guix_drv_path, &source)?;
        source.drv_path = drv_path.clone();
        source.out_path = out_path.clone();

        self.url_sources.insert(guix_drv_path.to_string(), source);
        // The Guix drv maps to the fetchurl drv; the Guix output to its output.
        self.map.insert(guix_drv_path.to_string(), drv_path.clone());
        self.map.insert(out.path.clone(), out_path);
        Ok(drv_path)
    }

    /// Instantiate `pkgs.fetchurl { … }` (no build/fetch) and return its
    /// `(drv_path, out_path)`. The output path is fixed by the hash, so this is
    /// a pure path computation that just writes the `.drv` to the store. The
    /// expression is rendered by emit_nix, so the emitted `.nix` files evaluate
    /// to the exact same derivation by construction.
    /// The batched prefetch has usually computed the paths already; a cache
    /// miss falls back to a per-source eval, whose error names the source.
    fn fetchurl_paths(
        &self,
        guix_drv_path: &str,
        source: &UrlSource,
    ) -> Result<(String, String), String> {
        if let Some(paths) = self.prefetched.get(guix_drv_path) {
            return Ok(paths.value().clone());
        }

        let expr = format!(
            "let f = ({fetch}) {args}; in {{ drv = f.drvPath; out = f.outPath; }}",
            fetch = emit_nix::fetch_url_fn(&self.nixpkgs_rev),
            args = emit_nix::url_source_args(source),
        );
        let output = std::process::Command::new("nix")
            .args(["eval", "--impure", "--json", "--expr", &expr])
            .output()
            .map_err(|e| format!("download {}: running nix eval: {e}", source.name))?;
        if !output.status.success() {
            return Err(format!(
                "download {}: instantiating pkgs.fetchurl failed:\n{}",
                source.name,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let v: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("download {}: parse fetchurl output: {e}", source.name))?;
        let drv = v["drv"]
            .as_str()
            .ok_or_else(|| format!("download {}: no drvPath", source.name))?
            .to_string();
        let out = v["out"]
            .as_str()
            .ok_or_else(|| format!("download {}: no outPath", source.name))?
            .to_string();
        Ok((drv, out))
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

    // Tests the batched prefetch expression: one eval must bind the shared
    // fetchurl/fetchgit helpers once and project each source's
    // drvPath/outPath, keyed by its Guix .drv path.
    #[test]
    fn prefetch_expr_binds_helpers_and_keys_by_drv_path() {
        let entries = vec![
            PrefetchEntry {
                guix_drv_path: "/gnu/store/aaa-foo.tar.gz.drv".into(),
                call: "fetchurl { urls = [ \"https://x\" ]; }".into(),
            },
            PrefetchEntry {
                guix_drv_path: "/gnu/store/bbb-bar-checkout.drv".into(),
                call: "fetchgit { url = \"https://y\"; }".into(),
            },
        ];
        let expr = prefetch_expr("deadbeef", &entries);
        assert!(expr.contains("let fetchurl ="), "{expr}");
        assert!(expr.contains("github:NixOS/nixpkgs/deadbeef"), "{expr}");
        assert!(expr.contains(".fetchgit"), "{expr}");
        assert!(
            expr.contains("\"/gnu/store/aaa-foo.tar.gz.drv\" = let f = fetchurl"),
            "{expr}"
        );
        assert!(
            expr.contains("\"/gnu/store/bbb-bar-checkout.drv\" = let f = fetchgit"),
            "{expr}"
        );
        assert!(
            expr.contains("in { drv = f.drvPath; out = f.outPath; }"),
            "{expr}"
        );
    }

    // Tests that fetchgit_args renders the exact argument set the
    // per-source eval used to inline, so the batched prefetch and the
    // fallback instantiate the identical fetchgit derivation.
    #[test]
    fn fetchgit_args_renders_call_arguments() {
        let gs = GitSource {
            url: "https://github.com/x/y".into(),
            rev: "refs/tags/v1".into(),
            name: "y-checkout".into(),
            hash_sri: "sha256-AAAA".into(),
            submodules: true,
            drv_path: String::new(),
            out_path: String::new(),
        };
        assert_eq!(
            fetchgit_args(&gs),
            "{ url = \"https://github.com/x/y\"; rev = \"refs/tags/v1\"; \
             hash = \"sha256-AAAA\"; name = \"y-checkout\"; fetchSubmodules = true; }"
        );
    }

    #[test]
    fn fetchgit_revision_distinguishes_commits_and_tags() {
        assert_eq!(
            fetchgit_revision("0123456789abcdef0123456789abcdef01234567"),
            "0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(fetchgit_revision("e6246c9"), "e6246c9");
        assert_eq!(fetchgit_revision("v1.2.3"), "refs/tags/v1.2.3");
        assert_eq!(fetchgit_revision("20250605"), "refs/tags/20250605");
        assert_eq!(
            fetchgit_revision("refs/tags/already-normalized"),
            "refs/tags/already-normalized"
        );
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

    // The flat sha256 used across the URL-list tests, whose nix32 form appears
    // in the expected bordeaux CA-mirror URL below.
    const FLAT_HASH: &str = "ba621bff6adc2e9e381f5907e0e86ad22b191678404e1f2888a5a924fa02031d";
    const CA_URL: &str = "https://bordeaux.guix.gnu.org/file/hello-source/sha256/07830bx29ad5i0l1ykj0g0b1jayjdblf01sr3ww9wbnwdbzinqms";

    // Tests that a flat-hash download leads with the CA mirror, keyed on the
    // OUTPUT store name — not the URL basename. A GitHub tag archive proves the
    // keying in the wild: the URL basename `v0.2.2.tar.gz` 404s on bordeaux
    // while the output store name `guile-zlib-0.2.2.tar.gz` 200s. The upstream
    // declarations follow in order as build-time fallbacks.
    #[test]
    fn candidate_urls_lead_with_ca_mirror_keyed_on_store_name() {
        let urls = download_candidate_urls(
            "hello-source",
            FLAT_HASH,
            false,
            "\"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz\"",
        )
        .unwrap();
        assert_eq!(
            urls,
            vec![
                CA_URL.to_string(),
                "https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz".to_string(),
            ]
        );
    }

    // Tests that a NAR-hashed (recursive/executable) download skips the CA
    // mirror — its lookup key is the flat file hash, which a NAR hash never
    // matches — and keeps the deterministic upstream ranking.
    #[test]
    fn candidate_urls_skip_ca_mirror_for_nar_hashes() {
        let urls = download_candidate_urls(
            "hello-source",
            FLAT_HASH,
            true,
            "(\"mirror://gnu/mes/m.tar.gz\" \"https://lilypond.org/janneke/m.tar.gz\")",
        )
        .unwrap();
        assert_eq!(
            urls,
            vec![
                "https://ftp.gnu.org/gnu/mes/m.tar.gz".to_string(),
                "https://lilypond.org/janneke/m.tar.gz".to_string(),
            ]
        );
    }

    // Tests that a download with no URL at all is a hard error rather than an
    // empty fetchurl `urls` list.
    #[test]
    fn candidate_urls_require_at_least_one_url() {
        assert!(download_candidate_urls("x", FLAT_HASH, true, "").is_err());
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
