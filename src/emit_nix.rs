//! Emit `.nix` files that reconstruct the translated derivation graph using
//! `builtins.derivation` calls.
//!
//! [`emit`] produces a single self-contained file using `let` bindings.
//!
//! [`emit_dir`] produces one file per derivation under `store/`. Each file is a
//! standalone `builtins.derivation { … }` that references its peers directly via
//! `(import ../store/<file>.nix).<output>`, so Nix tracks them as
//! `inputDrvs`/`inputSrcs` and computes the same hashes as `nix derivation add`.
//! Source inputs (`-builder`/`-source`/`-patch`) are `builtins.path` references.
//! Every store file also embeds a drvPath guard ([`guarded_source_file`]) that
//! throws at evaluation time if the file no longer produces the `.drv` whose
//! output paths consumers baked into their builder scripts.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use rayon::prelude::*;
use regex::Regex;

use crate::ast::{Derivation, derivation_name, store_path_name};
use crate::hash;
use crate::progress::{Mode, Progress};
use crate::splicer::{GitSource, UrlSource};

/// Regex matching a Nix store path (hash + name, no trailing slash/suffix).
static STORE_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/nix/store/[a-z0-9]{32}-[a-zA-Z0-9._?=+-]+").unwrap());

/// Environment variable that, when set to [`SKIP_DRV_GUARD_VALUE`], disables
/// the drvPath guard embedded in every emitted store file. Only
/// [`verify_consistency`] sets the env var: the check computes each drvPath
/// itself and needs the raw values for its aggregate mismatch report, which a
/// guard `throw` would cut short. `builtins.getEnv` returns `""` under pure
/// evaluation, so ordinary consumers can never trip the escape hatch.
pub const SKIP_DRV_GUARD_ENV: &str = "GUIX_TRANSFER_SKIP_DRV_GUARD";

/// The value [`SKIP_DRV_GUARD_ENV`] must hold to disable the guard.
pub const SKIP_DRV_GUARD_VALUE: &str = "1";

/// How many emitted `.nix` files a single `nix eval` process imports during
/// [`verify_consistency`]. Larger chunks amortize process startup and let
/// `import` memoization dedupe the dependency closures shared between files;
/// the cap still bounds evaluator memory (every imported derivation keeps
/// its builder-script env strings live for the life of the process). Chunks
/// evaluate in parallel across rayon workers.
const VERIFY_EVAL_CHUNK_SIZE: usize = 1000;

/// Data collected during splicer translation for one derivation.
pub struct TranslatedDrv {
    pub guix_drv_path: String,
    #[allow(dead_code)]
    pub nix_drv_path: String,
    /// The derivation after path-rewriting, with output paths blanked.
    pub drv: Derivation,
    /// Output name → computed Nix output path (from `nix derivation show`).
    pub nix_outputs: HashMap<String, String>,
}

/// Generate a single `.nix` file containing all translated derivations.
pub fn emit(
    out_path: &Path,
    translated: &[TranslatedDrv],
    url_sources: &[UrlSource],
    nixpkgs_rev: &str,
) -> Result<(), String> {
    let var_names = assign_var_names(translated);

    // Download sources become `fetchurl { … }` let-bindings; name them up
    // front so consumers can interpolate `${var}` for their output paths.
    let mut fetch_vars = Vec::with_capacity(url_sources.len());
    {
        let mut seen: HashMap<String, usize> = HashMap::new();
        for us in url_sources {
            let base = format!("fetch_{}", sanitize_ident(&us.name));
            let count = seen.entry(base.clone()).or_insert(0);
            let name = if *count == 0 {
                base.clone()
            } else {
                format!("{base}_{count}")
            };
            *count += 1;
            fetch_vars.push(name);
        }
    }

    // Reverse map: nix output path → (variable name, output name).
    let mut output_to_var: HashMap<&str, (&str, &str)> = HashMap::new();
    for (i, td) in translated.iter().enumerate() {
        for (out_name, out_path) in &td.nix_outputs {
            output_to_var.insert(out_path.as_str(), (&var_names[i], out_name.as_str()));
        }
    }
    for (i, us) in url_sources.iter().enumerate() {
        output_to_var.insert(us.out_path.as_str(), (&fetch_vars[i], "out"));
    }

    // Collect input sources that are NOT derivation outputs.
    let mut sources: HashMap<&str, String> = HashMap::new();
    let mut src_seen: HashMap<String, usize> = HashMap::new();
    for td in translated {
        for src in &td.drv.input_srcs {
            if output_to_var.contains_key(src.as_str()) || sources.contains_key(src.as_str()) {
                continue;
            }
            let base = format!("src_{}", sanitize_ident(store_path_name(src)));
            let count = src_seen.entry(base.clone()).or_insert(0);
            let name = if *count == 0 {
                base.clone()
            } else {
                format!("{base}_{count}")
            };
            *count += 1;
            sources.insert(src.as_str(), name);
        }
    }

    let mut nix = String::with_capacity(64 * 1024);
    nix.push_str("# Generated by guix-transfer\n");
    nix.push_str(
        "# Do not edit — regenerate with: guix-transfer --emit-nix <output.nix> <drv>\n\n",
    );
    nix.push_str("let\n");

    // Source bindings (builtins.storePath for non-drv store objects).
    for (path, var) in &sources {
        nix.push_str(&format!("  {var} = builtins.storePath {path};\n"));
    }
    if !sources.is_empty() {
        nix.push('\n');
    }

    // Download-source bindings: pinned nixpkgs fetchurl with the full
    // candidate URL list, so fallback happens at build time.
    if !url_sources.is_empty() {
        nix.push_str(&format!("  fetchurl = {};\n", fetch_url_fn(nixpkgs_rev)));
        for (i, us) in url_sources.iter().enumerate() {
            nix.push_str(&format!(
                "  {var} = fetchurl {args};\n",
                var = fetch_vars[i],
                args = url_source_args(us)
            ));
        }
        nix.push('\n');
    }

    // Derivation bindings.
    for (i, td) in translated.iter().enumerate() {
        let var = &var_names[i];
        let drv_name = derivation_name(&td.guix_drv_path);

        nix.push_str(&format!("  # {drv_name}\n"));
        nix.push_str(&format!("  {var} = builtins.derivation {{\n"));
        nix.push_str(&format!("    name = {q};\n", q = nix_str_literal(drv_name)));
        nix.push_str(&format!(
            "    system = {q};\n",
            q = nix_str_literal(&td.drv.system)
        ));
        nix.push_str(&format!(
            "    builder = {b};\n",
            b = interpolate(&td.drv.builder, &output_to_var, &sources)
        ));

        if !td.drv.args.is_empty() {
            nix.push_str("    args = [\n");
            for arg in &td.drv.args {
                nix.push_str(&format!(
                    "      {a}\n",
                    a = interpolate(arg, &output_to_var, &sources)
                ));
            }
            nix.push_str("    ];\n");
        }

        if !td.drv.input_srcs.is_empty() {
            nix.push_str("    srcs = [\n");
            for src in &td.drv.input_srcs {
                if let Some(var) = sources.get(src.as_str()) {
                    nix.push_str(&format!("      {var}\n"));
                }
            }
            nix.push_str("    ];\n");
        }

        // Multi-output declaration.
        let output_names: Vec<&str> = td.drv.outputs.iter().map(|o| o.name.as_str()).collect();
        if output_names.len() > 1 || output_names.first().copied() != Some("out") {
            let quoted: Vec<String> = output_names.iter().map(|n| format!("\"{n}\"")).collect();
            nix.push_str(&format!("    outputs = [{}];\n", quoted.join(" ")));
        }

        // Fixed-output derivation attributes.
        for out in &td.drv.outputs {
            if !out.hash.is_empty() {
                let executable = td.drv.env_get("executable") == Some("1");
                if let Ok(h) = hash::guix_to_nix(&out.hash_algo, &out.hash, executable) {
                    nix.push_str(&format!(
                        "    outputHash = {q};\n",
                        q = nix_str_literal(&h.sri)
                    ));
                    nix.push_str("    outputHashAlgo = \"sha256\";\n");
                    let mode = if h.method == "nar" {
                        "recursive"
                    } else {
                        &h.method
                    };
                    nix.push_str(&format!("    outputHashMode = \"{mode}\";\n"));
                }
            }
        }

        // Env vars.  The splicer injects `name`, `system`, `builder` into
        // env (matching what `builtins.derivation` does), so we skip them
        // here since they're already emitted as standard attributes above.
        // Output names and `outputHash*` are also handled separately, and
        // `srcs` is already emitted as a list above (the splicer injects a
        // matching `srcs` env var, which `builtins.derivation` re-derives by
        // flattening the list).
        let skip: &[&str] = &[
            "name",
            "system",
            "builder",
            "outputs",
            "outputHash",
            "outputHashAlgo",
            "outputHashMode",
            "srcs",
        ];
        for e in &td.drv.env {
            if skip.contains(&e.key.as_str()) {
                continue;
            }
            if output_names.contains(&e.key.as_str()) {
                continue;
            }
            nix.push_str(&format!(
                "    {k} = {v};\n",
                k = nix_attr_key(&e.key),
                v = interpolate(&e.value, &output_to_var, &sources)
            ));
        }

        nix.push_str("  };\n\n");
    }

    // A pure-download graph has no `builtins.derivation` bindings; its root is
    // the last fetchurl source.
    nix.push_str(&format!(
        "in\n  {}\n",
        var_names
            .last()
            .or(fetch_vars.last())
            .ok_or("no derivations to emit")?
    ));

    fs::write(out_path, &nix).map_err(|e| format!("write {}: {e}", out_path.display()))?;
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Assign a unique Nix variable name to each derivation.
fn assign_var_names(translated: &[TranslatedDrv]) -> Vec<String> {
    let mut names = Vec::with_capacity(translated.len());
    let mut seen: HashMap<String, usize> = HashMap::new();
    for td in translated {
        let base = sanitize_ident(derivation_name(&td.guix_drv_path));
        let count = seen.entry(base.clone()).or_insert(0);
        let name = if *count == 0 {
            base.clone()
        } else {
            format!("{base}_{count}")
        };
        *count += 1;
        names.push(name);
    }
    names
}

/// Turn an arbitrary string into a valid Nix identifier.
fn sanitize_ident(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    if s.is_empty() {
        s = "_unnamed".to_string();
    }
    s
}

/// Escape a string for use inside Nix `"..."` (no surrounding quotes).
fn escape_nix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                out.push_str("\\\\");
                i += 1;
            }
            b'"' => {
                out.push_str("\\\"");
                i += 1;
            }
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
                out.push_str("\\${");
                i += 2;
            }
            b'\n' => {
                out.push_str("\\n");
                i += 1;
            }
            b'\r' => {
                out.push_str("\\r");
                i += 1;
            }
            b'\t' => {
                out.push_str("\\t");
                i += 1;
            }
            _ => {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    out
}

/// A plain Nix string literal with no interpolation.
pub(crate) fn nix_str_literal(s: &str) -> String {
    format!("\"{}\"", escape_nix(s))
}

/// Produce a Nix string with `${var}` interpolation for known store paths.
fn interpolate(
    s: &str,
    output_to_var: &HashMap<&str, (&str, &str)>,
    sources: &HashMap<&str, String>,
) -> String {
    if !s.contains("/nix/store/") {
        return nix_str_literal(s);
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    for m in STORE_PATH_RE.find_iter(s) {
        let path = m.as_str();
        if let Some(&(var, out_name)) = output_to_var.get(path) {
            let interp = format!("${{{var}.{out_name}}}");
            replacements.push((m.start(), m.end(), interp));
        } else if let Some(var) = sources.get(path) {
            replacements.push((m.start(), m.end(), format!("${{{var}}}")));
        }
    }

    if replacements.is_empty() {
        return nix_str_literal(s);
    }

    let mut out = String::from("\"");
    let mut pos = 0;
    for (start, end, interp) in &replacements {
        out.push_str(&escape_nix(&s[pos..*start]));
        out.push_str(interp);
        pos = *end;
    }
    out.push_str(&escape_nix(&s[pos..]));
    out.push('"');
    out
}

/// Format an attribute key: bare if it's a valid Nix identifier, quoted otherwise.
fn nix_attr_key(key: &str) -> String {
    let valid = !key.is_empty()
        && key
            .chars()
            .next()
            .map(|c| c.is_alphabetic() || c == '_')
            .unwrap_or(false)
        && key
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '\'');
    if valid {
        key.to_string()
    } else {
        format!("\"{}\"", escape_nix(key))
    }
}

/// The `.nix` filename for a fetched source (download or git checkout):
/// `<hash>-<name>.nix` from its realized store path. Used both to write the
/// file and to reference it.
fn source_nix_filename(nix_path: &str) -> String {
    format!("{}.nix", store_path_name_with_hash(nix_path))
}

/// The store basename (`<hash>-<name>`) of a `/nix/store/...` path.
fn store_path_name_with_hash(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The shared `fetch-git.nix` helper: thin wrapper over a pinned nixpkgs'
/// `fetchgit`. nixpkgs is reached via `builtins.getFlake` with a pinned rev —
/// the only form that imports in pure flake evaluation (`import <store-path>` is
/// forbidden there, and `<nixpkgs>` needs NIX_PATH).
fn fetch_git_lib(nixpkgs_rev: &str) -> String {
    let flake = crate::splicer::nixpkgs_flake_ref(nixpkgs_rev);
    format!(
        "# Generated by guix-transfer. Fetcher for translated Guix git-fetch\n\
         # sources: pkgs.fetchgit reproduces Guix's git checkout exactly.\n\
         # getFlake is let-bound outside the lambda so one evaluator pays the\n\
         # nixpkgs evaluation once, however many checkouts it instantiates\n\
         # (builtins.getFlake is not memoized across calls).\n\
         let\n\
         \x20 pkgs = (builtins.getFlake {flake:?}).legacyPackages.x86_64-linux;\n\
         in\n\
         {{\n\
         \x20 url,\n\
         \x20 rev,\n\
         \x20 hash,\n\
         \x20 name,\n\
         \x20 fetchSubmodules ? false,\n\
         }}:\n\
         pkgs.fetchgit {{\n\
         \x20 inherit\n\
         \x20   url\n\
         \x20   rev\n\
         \x20   hash\n\
         \x20   name\n\
         \x20   fetchSubmodules\n\
         \x20   ;\n\
         }}\n"
    )
}

/// The bare fetchurl wrapper function over a pinned nixpkgs, shared verbatim
/// by [`fetch_url_lib`] (written to `fetch-url.nix`), the single-file emitter,
/// and the splicer's translation-time `nix eval` — one definition, so all of
/// them evaluate to the identical derivation by construction.
///
/// `fetchurl { urls = […]; }` tries each URL in order during the fixed-output
/// build, so a dead mirror never changes the derivation, only which URL ends
/// up satisfying it.
///
/// The getFlake sits in a `let` OUTSIDE the lambda, deliberately:
/// `builtins.getFlake` is not memoized across calls, so a body-level getFlake
/// re-evaluates nixpkgs on every application (~0.2s and hundreds of MB per
/// source), while the let-bound thunk is forced once per evaluator no matter
/// how many sources it instantiates.
pub fn fetch_url_fn(nixpkgs_rev: &str) -> String {
    let flake = crate::splicer::nixpkgs_flake_ref(nixpkgs_rev);
    format!(
        "let\n\
         \x20 pkgs = (builtins.getFlake {flake:?}).legacyPackages.x86_64-linux;\n\
         in\n\
         {{\n\
         \x20 urls,\n\
         \x20 hash,\n\
         \x20 name,\n\
         \x20 recursiveHash ? false,\n\
         \x20 executable ? false,\n\
         }}:\n\
         pkgs.fetchurl {{\n\
         \x20 inherit\n\
         \x20   urls\n\
         \x20   hash\n\
         \x20   name\n\
         \x20   recursiveHash\n\
         \x20   executable\n\
         \x20   ;\n\
         }}\n"
    )
}

/// The shared `fetch-url.nix` helper file: [`fetch_url_fn`] plus a header.
fn fetch_url_lib(nixpkgs_rev: &str) -> String {
    format!(
        "# Generated by guix-transfer. Fetcher for translated Guix downloads:\n\
         # pkgs.fetchurl tries each candidate URL in order at build time, so a\n\
         # dead mirror never changes the derivation identity.\n\
         {}",
        fetch_url_fn(nixpkgs_rev)
    )
}

/// The argument set for one download source. Shared by the emitted `.nix`
/// forms and the splicer's `nix eval` instantiation so they agree exactly.
pub fn url_source_args(us: &UrlSource) -> String {
    let urls = us
        .urls
        .iter()
        .map(|u| nix_str_literal(u))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{{ urls = [ {urls} ]; hash = {hash}; name = {name}; recursiveHash = {rec}; executable = {exe}; }}",
        hash = nix_str_literal(&us.hash_sri),
        name = nix_str_literal(&us.name),
        rec = if us.recursive { "true" } else { "false" },
        exe = if us.executable { "true" } else { "false" },
    )
}

/// Wrap a store-file derivation expression in the drvPath self-check guard.
///
/// Consumers bake this derivation's output paths into their builder scripts,
/// so the emitted expression must evaluate to exactly `expected_drv_path` —
/// the `.drv` recorded at sync time by `nix derivation add` (or the fetchurl
/// instantiation). The guard turns any drift into a hard, descriptive eval
/// failure at the point of use, long after the sync ran, instead of silently
/// building a derivation nothing references. [`verify_consistency`] disables
/// the guard via [`SKIP_DRV_GUARD_ENV`] to compare drvPaths itself.
fn guarded_source_file(header: &str, drv_expr: &str, expected_drv_path: &str) -> String {
    format!(
        "{header}\
         let\n\
         \x20 drv = {drv_expr};\n\
         \n\
         \x20 # The .drv recorded when this tree was generated; consumer builder\n\
         \x20 # scripts bake in output paths of exactly that derivation. getEnv\n\
         \x20 # returns \"\" in pure evaluation — the escape hatch exists only for\n\
         \x20 # guix-transfer's own consistency check.\n\
         \x20 expected = {expected};\n\
         in\n\
         if builtins.getEnv \"{env}\" == \"{val}\" || drv.drvPath == expected then\n\
         \x20 drv\n\
         else\n\
         \x20 throw \"guix-transfer: ${{builtins.baseNameOf expected}} evaluates to ${{drv.drvPath}} but consumers expect ${{expected}}; the emitted tree is inconsistent — regenerate it with guix-transfer\"\n",
        expected = nix_str_literal(expected_drv_path),
        env = SKIP_DRV_GUARD_ENV,
        val = SKIP_DRV_GUARD_VALUE,
    )
}

/// Render a download-source `.nix`: a `pkgs.fetchurl` call (via `fetch-url.nix`),
/// guarded against drifting from the `.drv` instantiated at translation time.
fn url_source_nix(us: &UrlSource) -> String {
    guarded_source_file(
        "# Generated by guix-transfer (download source)\n",
        &format!("(import ../fetch-url.nix) {}", url_source_args(us)),
        &us.drv_path,
    )
}

/// Render a git-source `.nix`: a `pkgs.fetchgit` call (via `fetch-git.nix`).
/// fetchgit is a build-time FOD that reproduces Guix's git checkout, fetched
/// lazily by the daemon (and cacheable) rather than during the sync.
fn git_source_nix(gs: &GitSource) -> String {
    format!(
        "# Generated by guix-transfer (git source)\n(import ../fetch-git.nix) {{ url = {url}; rev = {rev}; hash = {hash}; name = {name}; fetchSubmodules = {sub}; }}\n",
        url = nix_str_literal(&gs.url),
        rev = nix_str_literal(&gs.rev),
        hash = nix_str_literal(&gs.hash_sri),
        name = nix_str_literal(&gs.name),
        sub = if gs.submodules { "true" } else { "false" },
    )
}

/// Generate a directory containing all translated derivations as separate .nix files and sources.
pub fn emit_dir(
    out_dir: &Path,
    translated: &[TranslatedDrv],
    map: &std::collections::HashMap<String, String>,
    url_sources: &HashMap<String, UrlSource>,
    git_sources: &HashMap<String, GitSource>,
    nixpkgs_rev: &str,
) -> Result<(), String> {
    let store_dir = out_dir.join("store");
    let sources_dir = out_dir.join("sources");

    if store_dir.exists() {
        remove_dir_all_force(&store_dir).map_err(|e| format!("clean store dir: {e}"))?;
    }
    if sources_dir.exists() {
        remove_dir_all_force(&sources_dir).map_err(|e| format!("clean sources dir: {e}"))?;
    }

    fs::create_dir_all(&store_dir).map_err(|e| format!("create store dir: {e}"))?;
    fs::create_dir_all(&sources_dir).map_err(|e| format!("create sources dir: {e}"))?;

    // Map output/drv paths to (drv_filename, output_name)
    let mut output_to_file: HashMap<String, (String, String)> = HashMap::new();
    for td in translated {
        let drv_filename = Path::new(&td.nix_drv_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace(".drv", ".nix");
        output_to_file.insert(
            td.nix_drv_path.clone(),
            (drv_filename.clone(), "drvPath".to_string()),
        );
        for (out_name, out_path) in &td.nix_outputs {
            output_to_file.insert(out_path.clone(), (drv_filename.clone(), out_name.clone()));
        }
    }

    // Download sources: each becomes a `pkgs.fetchurl` FOD in its own `.nix`,
    // referenced like any derivation via `(import <file>).out`. Emit the shared
    // `fetch-url.nix` helper once, then a per-source file. Register both the
    // drv and output paths in output_to_file so consumers resolve either.
    if !url_sources.is_empty() {
        fs::write(out_dir.join("fetch-url.nix"), fetch_url_lib(nixpkgs_rev))
            .map_err(|e| format!("write fetch-url.nix: {e}"))?;
    }
    for entry in url_sources.values() {
        let filename = source_nix_filename(&entry.out_path);
        output_to_file.insert(
            entry.out_path.clone(),
            (filename.clone(), "out".to_string()),
        );
        output_to_file.insert(
            entry.drv_path.clone(),
            (filename.clone(), "drvPath".to_string()),
        );
        fs::write(store_dir.join(&filename), url_source_nix(entry))
            .map_err(|e| format!("write download source {filename}: {e}"))?;
    }

    // Git sources: same shape as download sources, via `fetch-git.nix`.
    if !git_sources.is_empty() {
        fs::write(out_dir.join("fetch-git.nix"), fetch_git_lib(nixpkgs_rev))
            .map_err(|e| format!("write fetch-git.nix: {e}"))?;
    }
    for entry in git_sources.values() {
        let filename = source_nix_filename(&entry.out_path);
        output_to_file.insert(
            entry.out_path.clone(),
            (filename.clone(), "out".to_string()),
        );
        output_to_file.insert(
            entry.drv_path.clone(),
            (filename.clone(), "drvPath".to_string()),
        );
        fs::write(store_dir.join(&filename), git_source_nix(entry))
            .map_err(|e| format!("write git source {filename}: {e}"))?;
    }

    // Copy unique sources to sources/.
    let mut copied_sources = std::collections::HashSet::new();
    for td in translated {
        for src_path in &td.drv.input_srcs {
            if copied_sources.contains(src_path) {
                continue;
            }
            copied_sources.insert(src_path.clone());
            let src_name = Path::new(src_path).file_name().unwrap().to_str().unwrap();
            let dest = sources_dir.join(src_name);
            let nix_path = map
                .get(src_path)
                .cloned()
                .unwrap_or_else(|| src_path.to_string());
            if let Err(e) = copy_recursive(Path::new(src_path), &dest) {
                eprintln!("WARNING: failed to copy source {nix_path}: {e}");
            }
        }
    }

    // Write each translated derivation to its own file, wrapped in the
    // drvPath guard pinning the `.drv` that `nix derivation add` produced.
    for td in translated {
        let drv_filename = Path::new(&td.nix_drv_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace(".drv", ".nix");
        let drv_name = derivation_name(&td.guix_drv_path);

        let mut attrs = String::with_capacity(4096);
        attrs.push_str(&format!("    name = {q};\n", q = nix_str_literal(drv_name)));
        attrs.push_str(&format!(
            "    system = {q};\n",
            q = nix_str_literal(&td.drv.system)
        ));
        attrs.push_str(&format!(
            "    builder = {b};\n",
            b = interpolate_multi(&td.drv.builder, &output_to_file)
        ));

        if !td.drv.args.is_empty() {
            attrs.push_str("    args = [\n");
            for arg in &td.drv.args {
                attrs.push_str(&format!(
                    "      {a}\n",
                    a = interpolate_multi(arg, &output_to_file)
                ));
            }
            attrs.push_str("    ];\n");
        }

        if !td.drv.input_srcs.is_empty() {
            attrs.push_str("    srcs = [\n");
            for src in &td.drv.input_srcs {
                let src_name = Path::new(src).file_name().unwrap().to_str().unwrap();
                let name = crate::ast::store_path_name(src);
                attrs.push_str(&format!(
                    "      (builtins.path {{ name = \"{name}\"; path = ./../sources/{src_name}; }})\n"
                ));
            }
            attrs.push_str("    ];\n");
        }

        let output_names: Vec<&str> = td.drv.outputs.iter().map(|o| o.name.as_str()).collect();
        if output_names.len() > 1 || output_names.first().copied() != Some("out") {
            let quoted: Vec<String> = output_names.iter().map(|n| format!("\"{n}\"")).collect();
            attrs.push_str(&format!("    outputs = [{}];\n", quoted.join(" ")));
        }

        for out in &td.drv.outputs {
            if !out.hash.is_empty() {
                let executable = td.drv.env_get("executable") == Some("1");
                if let Ok(h) = hash::guix_to_nix(&out.hash_algo, &out.hash, executable) {
                    attrs.push_str(&format!(
                        "    outputHash = {q};\n",
                        q = nix_str_literal(&h.sri)
                    ));
                    attrs.push_str("    outputHashAlgo = \"sha256\";\n");
                    let mode = if h.method == "nar" {
                        "recursive"
                    } else {
                        &h.method
                    };
                    attrs.push_str(&format!("    outputHashMode = \"{mode}\";\n"));
                }
            }
        }

        let skip: &[&str] = &[
            "name",
            "system",
            "builder",
            "outputs",
            "outputHash",
            "outputHashAlgo",
            "outputHashMode",
            "srcs",
        ];
        for e in &td.drv.env {
            if skip.contains(&e.key.as_str()) {
                continue;
            }
            if output_names.contains(&e.key.as_str()) {
                continue;
            }
            attrs.push_str(&format!(
                "    {k} = {v};\n",
                k = nix_attr_key(&e.key),
                v = interpolate_multi(&e.value, &output_to_file)
            ));
        }

        let nix = guarded_source_file(
            "# Generated by guix-transfer\n",
            &format!("builtins.derivation {{\n{attrs}  }}"),
            &td.nix_drv_path,
        );

        let file_path = store_dir.join(&drv_filename);
        fs::write(&file_path, &nix).map_err(|e| format!("write {}: {e}", file_path.display()))?;
    }

    Ok(())
}

/// After [`emit_dir`], verify that every emitted `.nix` evaluates to the *same*
/// derivation path that `nix derivation add` produced during translation.
///
/// guix-transfer computes each package's store path twice: once via
/// `nix derivation add` (whose output paths get baked into *consumer* builder
/// scripts through the rewrite map) and once via the emitted `builtins.derivation`
/// `.nix` file (what Nix actually builds). These MUST agree. If they diverge for
/// some derivations — e.g. a multi-output env-var mismatch between [`crate::json`]
/// and this module — every consumer bakes a store path that is never built, and
/// the tree is silently "split-brain" (classic symptom downstream:
/// `ld: cannot find crt1.o` / `-lc`). This check turns that silent corruption
/// into a hard, descriptive failure at sync time.
///
/// Download sources are covered too: their emitted `fetch-url.nix` call must
/// evaluate to the same fetchurl `.drv` the splicer instantiated during
/// translation (whose output path consumers bake in).
pub fn verify_consistency(
    out_dir: &Path,
    translated: &[TranslatedDrv],
    url_sources: &[UrlSource],
    progress_mode: Mode,
) -> Result<(), String> {
    use std::process::Command;

    let store_dir = out_dir.join("store");
    let store_abs = fs::canonicalize(&store_dir)
        .map_err(|e| format!("canonicalize {}: {e}", store_dir.display()))?;
    let store_abs = store_abs.to_str().ok_or("non-utf8 store path")?;

    // filename (`<hash>-<name>.nix`) -> expected `.drv` path from `nix derivation add`.
    let mut expected: HashMap<String, String> = HashMap::new();
    for td in translated {
        let fname = Path::new(&td.nix_drv_path)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("bad nix_drv_path")?
            .replace(".drv", ".nix");
        expected.insert(fname, td.nix_drv_path.clone());
    }
    for us in url_sources {
        expected.insert(source_nix_filename(&us.out_path), us.drv_path.clone());
    }

    // Evaluate the expected files' drvPaths in parallel fixed-size chunks,
    // one `nix eval` process per chunk. The chunk size bounds evaluator
    // memory; chunks run concurrently on rayon workers. Only files the
    // expected map names are imported — git-source files carry no baked
    // `.drv` path to check. The skip env var disables each file's embedded
    // drvPath guard: on drift the guard would throw at the first bad file,
    // cutting the aggregate report short — this check compares the raw
    // drvPaths itself. The eval runs read-only: the check only needs the
    // path computation, and read-only mode elides the per-derivation daemon
    // roundtrip that would re-register `.drv`s translation already wrote.
    let mut names: Vec<&str> = expected.keys().map(|s| s.as_str()).collect();
    names.sort_unstable();

    let progress = Progress::new(progress_mode, names.len());
    let chunk_results: Result<Vec<HashMap<String, Option<String>>>, String> = names
        .par_chunks(VERIFY_EVAL_CHUNK_SIZE)
        .map(|chunk| {
            // Claim the whole chunk up front, labeled by its leading file, so
            // the pause while `nix eval` runs is attributed to something
            // visible.
            progress.step_many(chunk.len(), chunk[0]);

            // A missing file would abort the chunk's eval with an import
            // error; leave absent files out so they surface as mismatches
            // below.
            let present: Vec<&str> = chunk
                .iter()
                .copied()
                .filter(|n| store_dir.join(n).is_file())
                .collect();
            if present.is_empty() {
                return Ok(HashMap::new());
            }

            let list = present
                .iter()
                .map(|n| nix_str_literal(n))
                .collect::<Vec<_>>()
                .join(" ");
            let expr = format!(
                "let d = {store_abs}; in builtins.listToAttrs (map (n: {{ name = n; value = let v = import (d + (\"/\" + n)); in if builtins.isAttrs v && v ? drvPath then v.drvPath else null; }}) [ {list} ])"
            );
            let output = Command::new("nix")
                .args(["eval", "--read-only", "--impure", "--json", "--expr", &expr])
                .env(SKIP_DRV_GUARD_ENV, SKIP_DRV_GUARD_VALUE)
                .output()
                .map_err(|e| format!("running `nix eval` for consistency check: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "consistency check could not evaluate the emitted store:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            serde_json::from_slice(&output.stdout)
                .map_err(|e| format!("parsing consistency eval output: {e}"))
        })
        .collect();

    let mut actual: HashMap<String, Option<String>> = HashMap::new();
    for chunk_actual in chunk_results? {
        actual.extend(chunk_actual);
    }
    progress.done();

    let mut mismatches: Vec<(String, String, String)> = Vec::new();
    for (fname, exp) in &expected {
        match actual.get(fname).and_then(|o| o.as_deref()) {
            Some(act) if act == exp => {}
            Some(act) => mismatches.push((fname.clone(), exp.clone(), act.to_string())),
            None => mismatches.push((fname.clone(), exp.clone(), String::new())),
        }
    }
    mismatches.sort();

    if mismatches.is_empty() {
        return Ok(());
    }

    let mut report = String::new();
    for (fname, exp, act) in &mismatches {
        if act.is_empty() {
            report.push_str(&format!("  {fname}: emitted .nix missing from store dir\n"));
        } else {
            report.push_str(&format!(
                "  {fname}\n      nix derivation add (baked into consumers): {exp}\n      emitted .nix evaluates to                 : {act}\n"
            ));
        }
    }

    // For the first mismatch with both .drv paths available, dump the exact
    // structural diff (builder/args/env/inputDrvs/inputSrcs/outputs). D1
    // exists from `nix derivation add`; D2 does not — the verification eval
    // ran read-only — so re-evaluate that one file writably (best effort) to
    // put D2 in the store where `nix derivation show` can see it. The diff
    // pinpoints WHICH field diverges — the precise signal needed to fix
    // json.rs / emit_nix without guessing.
    if let Some((fname, d1, d2)) = mismatches.iter().find(|(_, _, a)| !a.is_empty()) {
        let expr = format!("(import {store_abs}/{fname}).drvPath");
        let _ = Command::new("nix")
            .args(["eval", "--impure", "--raw", "--expr", &expr])
            .env(SKIP_DRV_GUARD_ENV, SKIP_DRV_GUARD_VALUE)
            .output();

        if let Some(diff) = diff_drvs(d1, d2) {
            report.push_str(&format!(
                "\n── structural diff of the first mismatch ({fname}) ──\n  D1 = nix derivation add (baked path): {d1}\n  D2 = emitted .nix (built path)      : {d2}\n{diff}"
            ));
        }
    }

    Err(format!(
        "CONSISTENCY CHECK FAILED: {} derivation(s) whose `nix derivation add` path (baked into \
         consumer builder scripts via the rewrite map) does not match the path the emitted `.nix` \
         actually builds. Consumers would reference store paths that are never built (classic \
         downstream symptom: `ld: cannot find crt1.o` / `-lc`). This means json.rs and emit_nix \
         disagree for these derivations.\n{}",
        mismatches.len(),
        report
    ))
}

/// `nix derivation show` a `.drv` and return its single derivation object.
fn show_drv(path: &str) -> Option<serde_json::Value> {
    let out = std::process::Command::new("nix")
        .args(["derivation", "show", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    // { "<path>": {...} } or { "derivations": { "<path>": {...} } }
    let map = v.get("derivations").unwrap_or(&v).as_object()?;
    map.values().next().cloned()
}

/// Produce a human-readable structural diff between two derivations (D1 vs D2),
/// reporting only the fields that differ. Returns None if either can't be shown.
fn diff_drvs(d1_path: &str, d2_path: &str) -> Option<String> {
    let a = show_drv(d1_path)?;
    let b = show_drv(d2_path)?;
    let mut out = String::new();

    // env: compare key sets and values (ignore output-name vars, which Nix
    // blanks during hashing, to avoid noise).
    let empty = serde_json::Map::new();
    let ea = a.get("env").and_then(|v| v.as_object()).unwrap_or(&empty);
    let eb = b.get("env").and_then(|v| v.as_object()).unwrap_or(&empty);
    let out_names: std::collections::HashSet<&str> = a
        .get("outputs")
        .and_then(|v| v.as_object())
        .map(|o| o.keys().map(|s| s.as_str()).collect())
        .unwrap_or_default();
    let mut env_lines = Vec::new();
    let mut keys: Vec<&String> = ea.keys().chain(eb.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        if out_names.contains(k.as_str()) {
            continue;
        }
        let va = ea.get(k).and_then(|v| v.as_str());
        let vb = eb.get(k).and_then(|v| v.as_str());
        if va != vb {
            env_lines.push(format!(
                "      env[{k}]:\n        D1: {}\n        D2: {}",
                va.unwrap_or("<absent>"),
                vb.unwrap_or("<absent>")
            ));
        }
    }
    if !env_lines.is_empty() {
        out.push_str("  env differs:\n");
        out.push_str(&env_lines.join("\n"));
        out.push('\n');
    }

    // inputDrvs, inputSrcs, args, builder
    for field in ["builder"] {
        if a.get(field) != b.get(field) {
            out.push_str(&format!(
                "  {field} differs:\n    D1: {}\n    D2: {}\n",
                a.get(field).map(|v| v.to_string()).unwrap_or_default(),
                b.get(field).map(|v| v.to_string()).unwrap_or_default()
            ));
        }
    }
    if a.get("args") != b.get("args") {
        out.push_str("  args differ\n");
    }

    // inputs.drvs: compare per-input output sets
    let ia = a
        .get("inputs")
        .and_then(|i| i.get("drvs"))
        .and_then(|v| v.as_object());
    let ib = b
        .get("inputs")
        .and_then(|i| i.get("drvs"))
        .and_then(|v| v.as_object());
    if let (Some(ia), Some(ib)) = (ia, ib) {
        let mut dk: Vec<&String> = ia.keys().chain(ib.keys()).collect();
        dk.sort();
        dk.dedup();
        let mut lines = Vec::new();
        for k in dk {
            let oa = ia.get(k).and_then(|v| v.get("outputs"));
            let ob = ib.get(k).and_then(|v| v.get("outputs"));
            if oa != ob {
                lines.push(format!(
                    "      {k}\n        D1 outputs: {}\n        D2 outputs: {}",
                    oa.map(|v| v.to_string()).unwrap_or("<absent>".into()),
                    ob.map(|v| v.to_string()).unwrap_or("<absent>".into())
                ));
            }
        }
        if !lines.is_empty() {
            out.push_str("  inputs.drvs differ (input → outputs used):\n");
            out.push_str(&lines.join("\n"));
            out.push('\n');
        }
    }

    // inputs.srcs
    let sa = a.get("inputs").and_then(|i| i.get("srcs"));
    let sb = b.get("inputs").and_then(|i| i.get("srcs"));
    if sa != sb {
        out.push_str(&format!(
            "  inputs.srcs differ:\n    D1: {}\n    D2: {}\n",
            sa.map(|v| v.to_string()).unwrap_or_default(),
            sb.map(|v| v.to_string()).unwrap_or_default()
        ));
    }

    if out.is_empty() {
        out.push_str("  (no field-level diff found — drvs differ only via input-path hashes)\n");
    }
    Some(out)
}

#[allow(clippy::permissions_set_readonly_false)]
fn copy_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = fs::metadata(src).map_err(|e| format!("stat {}: {e}", src.display()))?;
    if meta.is_dir() {
        fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
        for entry in fs::read_dir(src).map_err(|e| format!("read dir {}: {e}", src.display()))? {
            let entry = entry.map_err(|e| format!("read entry: {e}"))?;
            let entry_path = entry.path();
            let dest_path = dst.join(entry.file_name());
            copy_recursive(&entry_path, &dest_path)?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        fs::copy(src, dst)
            .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
        let mut perms = fs::metadata(dst)
            .map_err(|e| format!("stat {}: {e}", dst.display()))?
            .permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            fs::set_permissions(dst, perms)
                .map_err(|e| format!("set perms on {}: {e}", dst.display()))?;
        }
    }
    Ok(())
}

fn remove_dir_all_force(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    set_writeable_recursive(path)?;
    fs::remove_dir_all(path)
}

#[allow(clippy::permissions_set_readonly_false)]
fn set_writeable_recursive(path: &Path) -> std::io::Result<()> {
    let meta = fs::metadata(path)?;
    let mut perms = meta.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        fs::set_permissions(path, perms)?;
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            set_writeable_recursive(&entry.path())?;
        }
    }
    Ok(())
}

fn interpolate_multi(s: &str, output_to_file: &HashMap<String, (String, String)>) -> String {
    if !s.contains("/nix/store/") {
        return nix_str_literal(s);
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    for m in STORE_PATH_RE.find_iter(s) {
        let path = m.as_str();
        if let Some((drv_filename, out_name)) = output_to_file.get(path) {
            let interp = if out_name == "drvPath" {
                format!("${{(import ../store/{drv_filename}).drvPath}}")
            } else {
                format!("${{(import ../store/{drv_filename}).{out_name}}}")
            };
            replacements.push((m.start(), m.end(), interp));
        } else if path.contains("-source")
            || path.contains("-builder")
            || path.contains("-patch")
            || !path.ends_with(".drv")
        {
            let filename = Path::new(path).file_name().unwrap().to_str().unwrap();
            let name = crate::ast::store_path_name(path);
            replacements.push((
                m.start(),
                m.end(),
                format!(
                    "${{builtins.path {{ name = ''{name}''; path = ./../sources/{filename}; }}}}"
                ),
            ));
        }
    }

    if replacements.is_empty() {
        return nix_str_literal(s);
    }

    let mut out = String::from("\"");
    let mut pos = 0;
    for (start, end, interp) in &replacements {
        out.push_str(&escape_nix(&s[pos..*start]));
        out.push_str(interp);
        pos = *end;
    }
    out.push_str(&escape_nix(&s[pos..]));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{EnvVar, Output};

    /// Regression test for the "split-brain tree" bug.
    ///
    /// guix-transfer serializes each derivation two ways that MUST agree:
    ///   * `json::to_nix_json` → `nix derivation add` — the path baked into
    ///     consumer builder scripts via the rewrite map.
    ///   * `emit_nix::emit` → `builtins.derivation` — the `.nix` Nix actually
    ///     builds.
    ///
    /// These once diverged for *multi-output* derivations (an env-var mismatch),
    /// so consumers baked a glibc path that was never built — surfacing far
    /// downstream as `ld: cannot find crt1.o` / `-lc`. This test feeds one
    /// 3-output derivation through both paths and asserts the resulting `.drv`
    /// paths are identical. Requires `nix` on PATH.
    #[test]
    fn multi_output_json_and_nix_agree() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // This test shells out to `nix derivation add`/`nix eval`. Inside the Nix
        // build sandbox (`nix build .#default`'s check phase) there is no `nix` on
        // PATH, so skip rather than fail there; it still runs in the dev shell/CI
        // where nix is available.
        if Command::new("nix").arg("--version").output().is_err() {
            eprintln!("skipping multi_output_json_and_nix_agree: `nix` not on PATH");
            return;
        }

        // Mirror what the splicer hands the serializers: name/system/builder
        // injected into env, output paths blanked, output-name env vars present
        // but empty.
        let guix_drv = "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-multi-test.drv";
        let mk_out = |n: &str| Output {
            name: n.into(),
            path: String::new(),
            hash_algo: String::new(),
            hash: String::new(),
        };
        let drv = Derivation {
            outputs: vec![mk_out("debug"), mk_out("out"), mk_out("static")],
            input_drvs: vec![],
            input_srcs: vec![],
            system: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: vec!["-c".into(), "echo hi > $out".into()],
            env: vec![
                EnvVar {
                    key: "name".into(),
                    value: "multi-test".into(),
                },
                EnvVar {
                    key: "system".into(),
                    value: "x86_64-linux".into(),
                },
                EnvVar {
                    key: "builder".into(),
                    value: "/bin/sh".into(),
                },
                EnvVar {
                    key: "debug".into(),
                    value: String::new(),
                },
                EnvVar {
                    key: "out".into(),
                    value: String::new(),
                },
                EnvVar {
                    key: "static".into(),
                    value: String::new(),
                },
            ],
        };

        // Path A: json.rs → `nix derivation add` (what gets baked into consumers).
        let json = crate::json::to_nix_json(&drv, guix_drv).expect("to_nix_json");
        let mut add = Command::new("nix")
            .args(["derivation", "add"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `nix derivation add` (is nix on PATH?)");
        add.stdin
            .take()
            .unwrap()
            .write_all(json.to_string().as_bytes())
            .unwrap();
        let add_out = add.wait_with_output().unwrap();
        assert!(
            add_out.status.success(),
            "nix derivation add failed: {}",
            String::from_utf8_lossy(&add_out.stderr)
        );
        let path_a = String::from_utf8(add_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Path B: emit_nix → `builtins.derivation` (what Nix actually builds).
        let dir = std::env::temp_dir().join(format!("gt-consistency-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let nix_file = dir.join("multi-test.nix");
        let td = TranslatedDrv {
            guix_drv_path: guix_drv.to_string(),
            nix_drv_path: format!("/nix/store/{}.drv", "x".repeat(32) + "-multi-test"),
            drv: drv.clone(),
            nix_outputs: HashMap::new(),
        };
        emit(&nix_file, std::slice::from_ref(&td), &[], "unused").expect("emit");
        let eval = Command::new("nix")
            .args(["eval", "--impure", "--raw", "--expr"])
            .arg(format!("(import {}).drvPath", nix_file.display()))
            .output()
            .expect("spawn `nix eval`");
        assert!(
            eval.status.success(),
            "nix eval failed: {}",
            String::from_utf8_lossy(&eval.stderr)
        );
        let path_b = String::from_utf8(eval.stdout).unwrap().trim().to_string();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            path_a, path_b,
            "json.rs (nix derivation add) and emit_nix (builtins.derivation) produced \
             different .drv paths for a multi-output derivation — consumers would bake a \
             path that is never built (the split-brain bug)."
        );
    }

    /// A minimal splicer-shaped derivation (name/system/builder mirrored into
    /// env, output paths blanked, output-name env vars present but empty) that
    /// Nix can instantiate with /bin/sh, for tests that shell out to `nix`.
    fn splicer_style_drv(name: &str, output_names: &[&str]) -> Derivation {
        let mk_out = |n: &str| Output {
            name: n.into(),
            path: String::new(),
            hash_algo: String::new(),
            hash: String::new(),
        };
        let mut env = vec![
            EnvVar {
                key: "name".into(),
                value: name.into(),
            },
            EnvVar {
                key: "system".into(),
                value: "x86_64-linux".into(),
            },
            EnvVar {
                key: "builder".into(),
                value: "/bin/sh".into(),
            },
        ];
        for out in output_names {
            env.push(EnvVar {
                key: (*out).into(),
                value: String::new(),
            });
        }
        Derivation {
            outputs: output_names.iter().map(|n| mk_out(n)).collect(),
            input_drvs: vec![],
            input_srcs: vec![],
            system: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: vec!["-c".into(), "echo hi > $out".into()],
            env,
        }
    }

    /// Emit a one-derivation store dir (no sources) and return the path of the
    /// emitted `.nix` file for that derivation.
    fn emit_single_drv_dir(dir: &Path, td: &TranslatedDrv) -> std::path::PathBuf {
        emit_dir(
            dir,
            std::slice::from_ref(td),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            "unused",
        )
        .expect("emit_dir");
        let fname = Path::new(&td.nix_drv_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace(".drv", ".nix");
        dir.join("store").join(fname)
    }

    // Tests that emit_dir wraps each derivation file in the drvPath guard: the
    // expected `.drv` literal, the comparison against it, the env-var escape
    // hatch, and the throw arm must all appear in the emitted text.
    #[test]
    fn emitted_store_files_carry_drvpath_guard() {
        let expected_drv = format!("/nix/store/{}-guard-content.drv", "c".repeat(32));
        let td = TranslatedDrv {
            guix_drv_path: "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-guard-content.drv".into(),
            nix_drv_path: expected_drv.clone(),
            drv: splicer_style_drv("guard-content", &["out"]),
            nix_outputs: HashMap::new(),
        };

        let dir = std::env::temp_dir().join(format!("gt-guard-content-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let nix_file = emit_single_drv_dir(&dir, &td);
        let text = fs::read_to_string(&nix_file).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert!(text.contains("drv = builtins.derivation {"), "{text}");
        assert!(
            text.contains(&format!("expected = \"{expected_drv}\";")),
            "{text}"
        );
        assert!(text.contains("drv.drvPath == expected"), "{text}");
        assert!(text.contains(SKIP_DRV_GUARD_ENV), "{text}");
        assert!(text.contains("throw"), "{text}");
    }

    // Tests that download-source files carry the same drvPath guard, pinned to
    // the fetchurl `.drv` the splicer instantiated at translation time.
    #[test]
    fn url_source_nix_carries_drvpath_guard() {
        let s = url_source_nix(&url_source());
        assert!(
            s.contains("expected = \"/nix/store/d-hello-2.12.tar.gz.drv\";"),
            "{s}"
        );
        assert!(s.contains("drv.drvPath == expected"), "{s}");
        assert!(s.contains(SKIP_DRV_GUARD_ENV), "{s}");
    }

    // Tests that Nix enforces the embedded guard at evaluation time: a store
    // file whose recorded `.drv` path does not match what the expression
    // actually produces must throw when imported, and setting the skip env
    // var (as `verify_consistency` does) must bypass the guard. Requires
    // `nix` on PATH.
    #[test]
    fn drvpath_guard_throws_on_drift() {
        use std::process::Command;

        if Command::new("nix").arg("--version").output().is_err() {
            eprintln!("skipping drvpath_guard_throws_on_drift: `nix` not on PATH");
            return;
        }

        // Deliberately record a bogus expected .drv path for the derivation.
        let bogus_drv = format!("/nix/store/{}-guard-drift.drv", "x".repeat(32));
        let td = TranslatedDrv {
            guix_drv_path: "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-guard-drift.drv".into(),
            nix_drv_path: bogus_drv,
            drv: splicer_style_drv("guard-drift", &["out"]),
            nix_outputs: HashMap::new(),
        };

        let dir = std::env::temp_dir().join(format!("gt-guard-drift-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let nix_file = emit_single_drv_dir(&dir, &td);
        let expr = format!("(import {}).drvPath", nix_file.display());

        // Without the escape hatch the guard must throw.
        let eval = Command::new("nix")
            .args(["eval", "--impure", "--raw", "--expr", &expr])
            .output()
            .expect("spawn `nix eval`");
        assert!(
            !eval.status.success(),
            "guard did not throw on a drifted drvPath"
        );
        assert!(
            String::from_utf8_lossy(&eval.stderr).contains("guix-transfer"),
            "guard failure does not mention guix-transfer:\n{}",
            String::from_utf8_lossy(&eval.stderr)
        );

        // With the escape hatch set, the raw drvPath must come through.
        let eval = Command::new("nix")
            .args(["eval", "--impure", "--raw", "--expr", &expr])
            .env(SKIP_DRV_GUARD_ENV, SKIP_DRV_GUARD_VALUE)
            .output()
            .expect("spawn `nix eval`");
        let _ = fs::remove_dir_all(&dir);
        assert!(
            eval.status.success(),
            "skip env var did not bypass the guard: {}",
            String::from_utf8_lossy(&eval.stderr)
        );
    }

    /// Run a derivation's splicer-style JSON through `nix derivation add` and
    /// return the resulting `.drv` path — the same path the splicer records
    /// in [`TranslatedDrv::nix_drv_path`] during a real sync.
    fn nix_derivation_add(drv: &Derivation, guix_drv_path: &str) -> String {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let json = crate::json::to_nix_json(drv, guix_drv_path).expect("to_nix_json");
        let mut add = Command::new("nix")
            .args(["derivation", "add"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `nix derivation add` (is nix on PATH?)");
        add.stdin
            .take()
            .unwrap()
            .write_all(json.to_string().as_bytes())
            .unwrap();
        let add_out = add.wait_with_output().unwrap();
        assert!(
            add_out.status.success(),
            "nix derivation add failed: {}",
            String::from_utf8_lossy(&add_out.stderr)
        );
        String::from_utf8(add_out.stdout)
            .unwrap()
            .trim()
            .to_string()
    }

    // Tests verify_consistency end-to-end over an emitted store dir: the check
    // must pass when the recorded nix_drv_path matches what the emitted file
    // evaluates to, and fail with the aggregate mismatch report when the
    // recorded path drifts. The drifted file's embedded drvPath guard would
    // throw on import, so getting a mismatch report (rather than an eval
    // error) also proves the check runs with the guard disabled. Requires
    // `nix` on PATH.
    #[test]
    fn verify_consistency_detects_drift() {
        use std::process::Command;

        if Command::new("nix").arg("--version").output().is_err() {
            eprintln!("skipping verify_consistency_detects_drift: `nix` not on PATH");
            return;
        }

        let guix_drv = "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-verify-drift.drv";
        let drv = splicer_style_drv("verify-drift", &["out"]);
        let real_drv_path = nix_derivation_add(&drv, guix_drv);

        let dir = std::env::temp_dir().join(format!("gt-verify-drift-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        // Recorded path matches reality: the check must pass.
        let td = TranslatedDrv {
            guix_drv_path: guix_drv.to_string(),
            nix_drv_path: real_drv_path,
            drv: drv.clone(),
            nix_outputs: HashMap::new(),
        };
        emit_single_drv_dir(&dir, &td);
        verify_consistency(&dir, std::slice::from_ref(&td), &[], Mode::Auto)
            .expect("consistent tree");

        // Recorded path drifts: the check must fail with the aggregate report.
        let td = TranslatedDrv {
            nix_drv_path: format!("/nix/store/{}-verify-drift.drv", "x".repeat(32)),
            ..td
        };
        emit_single_drv_dir(&dir, &td);
        let err = verify_consistency(&dir, std::slice::from_ref(&td), &[], Mode::Auto)
            .expect_err("drifted tree must fail the check");
        let _ = fs::remove_dir_all(&dir);
        assert!(err.contains("CONSISTENCY CHECK FAILED"), "{err}");
    }

    #[test]
    fn sanitize_ident_basic() {
        assert_eq!(sanitize_ident("hello-2.12.2"), "hello_2_12_2");
        assert_eq!(sanitize_ident("0xdeadbeef"), "_0xdeadbeef");
        assert_eq!(
            sanitize_ident("gcc-core-mesboot0-2.95.3"),
            "gcc_core_mesboot0_2_95_3"
        );
    }

    #[test]
    fn escape_nix_basic() {
        assert_eq!(escape_nix("hello"), "hello");
        assert_eq!(escape_nix("a\"b"), "a\\\"b");
        assert_eq!(escape_nix("${foo}"), "\\${foo}");
        assert_eq!(escape_nix("$out"), "$out");
        assert_eq!(escape_nix("a\\b"), "a\\\\b");
    }

    #[test]
    fn interpolate_replaces_known_paths() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gcc",
            ("gcc", "out"),
        );
        let sources = HashMap::new();
        assert_eq!(
            interpolate(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gcc/bin/gcc",
                &outputs,
                &sources
            ),
            "\"${gcc.out}/bin/gcc\""
        );
    }

    #[test]
    fn interpolate_preserves_unknown_paths() {
        let outputs = HashMap::new();
        let sources = HashMap::new();
        assert_eq!(
            interpolate(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-unknown/bin/x",
                &outputs,
                &sources
            ),
            "\"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-unknown/bin/x\""
        );
    }

    #[test]
    fn interpolate_handles_shell_dollar_brace() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep",
            ("dep", "out"),
        );
        let sources = HashMap::new();
        assert_eq!(
            interpolate(
                "echo ${foo} /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep/bin/x",
                &outputs,
                &sources
            ),
            "\"echo \\${foo} ${dep.out}/bin/x\""
        );
    }

    #[test]
    fn interpolate_multi_output() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-pkg-lib",
            ("pkg", "lib"),
        );
        let sources = HashMap::new();
        assert_eq!(
            interpolate(
                "-L/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-pkg-lib/lib",
                &outputs,
                &sources
            ),
            "\"-L${pkg.lib}/lib\""
        );
    }

    #[test]
    fn interpolate_multi_refs_import_output() {
        let mut m = HashMap::new();
        m.insert(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gcc".to_string(),
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gcc.nix".to_string(),
                "out".to_string(),
            ),
        );
        assert_eq!(
            interpolate_multi(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gcc/bin/gcc",
                &m
            ),
            "\"${(import ../store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gcc.nix).out}/bin/gcc\""
        );
    }

    #[test]
    fn interpolate_multi_refs_import_drvpath() {
        let mut m = HashMap::new();
        m.insert(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-foo.drv".to_string(),
            (
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-foo.nix".to_string(),
                "drvPath".to_string(),
            ),
        );
        assert_eq!(
            interpolate_multi("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-foo.drv", &m),
            "\"${(import ../store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-foo.nix).drvPath}\""
        );
    }

    #[test]
    fn var_names_are_unique() {
        use crate::ast::Derivation;
        let mk = |name: &str| TranslatedDrv {
            guix_drv_path: format!("/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-{name}.drv"),
            nix_drv_path: String::new(),
            drv: Derivation {
                outputs: vec![],
                input_drvs: vec![],
                input_srcs: vec![],
                system: String::new(),
                builder: String::new(),
                args: vec![],
                env: vec![],
            },
            nix_outputs: HashMap::new(),
        };
        let tds = vec![
            mk("module-import-compiled"),
            mk("module-import-compiled"),
            mk("module-import-compiled"),
        ];
        let names = assign_var_names(&tds);
        assert_eq!(names[0], "module_import_compiled");
        assert_eq!(names[1], "module_import_compiled_1");
        assert_eq!(names[2], "module_import_compiled_2");
    }

    #[test]
    fn source_nix_filename_from_path() {
        assert_eq!(
            source_nix_filename("/nix/store/51ylhwb-libfaketime-0.9.10-checkout"),
            "51ylhwb-libfaketime-0.9.10-checkout.nix"
        );
    }

    fn url_source() -> UrlSource {
        UrlSource {
            urls: vec![
                "https://bordeaux.guix.gnu.org/file/hello-2.12.tar.gz/sha256/0783".into(),
                "https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz".into(),
            ],
            name: "hello-2.12.tar.gz".into(),
            hash_sri: "sha256-AAAA".into(),
            recursive: false,
            executable: false,
            drv_path: "/nix/store/d-hello-2.12.tar.gz.drv".into(),
            out_path: "/nix/store/h-hello-2.12.tar.gz".into(),
        }
    }

    // Tests that a download source renders as a fetch-url.nix call carrying
    // the ordered URL list and the hash mode flags.
    #[test]
    fn url_source_nix_renders_fetchurl_call() {
        let s = url_source_nix(&url_source());
        assert!(s.contains("(import ../fetch-url.nix)"));
        assert!(s.contains(
            "urls = [ \"https://bordeaux.guix.gnu.org/file/hello-2.12.tar.gz/sha256/0783\" \
             \"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz\" ]"
        ));
        assert!(s.contains("hash = \"sha256-AAAA\""));
        assert!(s.contains("name = \"hello-2.12.tar.gz\""));
        assert!(s.contains("recursiveHash = false"));
        assert!(s.contains("executable = false"));

        let nar_exe = UrlSource {
            recursive: true,
            executable: true,
            ..url_source()
        };
        let s2 = url_source_nix(&nar_exe);
        assert!(s2.contains("recursiveHash = true"));
        assert!(s2.contains("executable = true"));
    }

    // Tests that URLs are escaped as Nix string literals (quotes, backslashes,
    // and `${` interpolation cannot leak into the emitted expression).
    #[test]
    fn url_source_args_escape_nix_metacharacters() {
        let mut us = url_source();
        us.urls = vec![r#"https://example/a\b/"q"/${x}"#.into()];
        assert!(url_source_args(&us).contains(r#"urls = [ "https://example/a\\b/\"q\"/\${x}" ]"#));
    }

    // Also asserts the getFlake is hoisted into a `let` outside the lambda:
    // builtins.getFlake is not memoized across calls, so a body-level
    // getFlake re-evaluates nixpkgs per instantiated source (a ~40x
    // slowdown on batched evals and the source of multi-GB verify passes).
    #[test]
    fn fetch_url_fn_uses_getflake_pinned_rev() {
        let f = fetch_url_fn("deadbeef");
        assert!(f.contains("builtins.getFlake"));
        assert!(f.contains("github:NixOS/nixpkgs/deadbeef"));
        assert!(f.contains(".fetchurl"));
        assert!(f.starts_with("let\n"), "getFlake must be hoisted:\n{f}");
    }

    #[test]
    fn git_source_nix_renders_fetchgit_call() {
        let gs = GitSource {
            url: "https://github.com/x/y".into(),
            rev: "v1.2.3".into(),
            name: "foo-0.1-checkout".into(),
            hash_sri: "sha256-AAAA".into(),
            submodules: false,
            drv_path: "/nix/store/d-foo.drv".into(),
            out_path: "/nix/store/h-foo-0.1-checkout".into(),
        };
        let s = git_source_nix(&gs);
        assert!(s.contains("(import ../fetch-git.nix)"));
        assert!(s.contains("url = \"https://github.com/x/y\""));
        assert!(s.contains("rev = \"v1.2.3\""));
        assert!(s.contains("hash = \"sha256-AAAA\""));
        assert!(s.contains("name = \"foo-0.1-checkout\""));
        assert!(s.contains("fetchSubmodules = false"));

        let gs2 = GitSource {
            submodules: true,
            ..gs
        };
        assert!(git_source_nix(&gs2).contains("fetchSubmodules = true"));
    }

    // Also asserts the getFlake is hoisted into a `let` outside the lambda,
    // for the same reason as the fetch_url_fn test above.
    #[test]
    fn fetch_git_lib_uses_getflake_pinned_rev() {
        let lib = fetch_git_lib("deadbeef");
        assert!(lib.contains("builtins.getFlake"));
        assert!(lib.contains("github:NixOS/nixpkgs/deadbeef"));
        assert!(lib.contains(".fetchgit"));
        let let_pos = lib.find("let\n").expect("hoisted let");
        let lambda_pos = lib.find("url,").expect("lambda head");
        assert!(let_pos < lambda_pos, "getFlake must be hoisted:\n{lib}");
    }
}
