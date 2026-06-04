//! Thin wrappers over the `nix` / `nix-store` CLIs.
//!
//! Two distinct registration paths, for two distinct kinds of object:
//!   * derivations  → `nix derivation add` (computes the canonical `text:` path)
//!   * source files → `nix-store --add`     (a `source:` content-addressed path)

use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

const EXPERIMENTAL: &[&str] = &["--extra-experimental-features", "nix-command"];

/// Register a derivation from its JSON (format v4) and return its `.drv` path.
pub fn derivation_add(json: &Value) -> Result<String, String> {
    let mut child = Command::new("nix")
        .args(EXPERIMENTAL)
        .args(["derivation", "add"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `nix derivation add`: {e}"))?;

    let payload = serde_json::to_vec(json).map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&payload)
        .map_err(|e| format!("write to `nix derivation add`: {e}"))?;

    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "`nix derivation add` failed: {}\n--- json ---\n{}",
            String::from_utf8_lossy(&out.stderr),
            serde_json::to_string_pretty(json).unwrap_or_default()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Return the computed output paths (`output name → /nix/store/...`) of a
/// registered derivation.
pub fn output_paths(drv_path: &str) -> Result<HashMap<String, String>, String> {
    let out = Command::new("nix")
        .args(EXPERIMENTAL)
        .args(["derivation", "show", drv_path])
        .output()
        .map_err(|e| format!("spawn `nix derivation show`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`nix derivation show` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    // Top-level shape: {"derivations": {"<drv>": {...}}, "version": N}
    // or, in older versions, {"<drv>": {...}} directly.
    let root = v.get("derivations").unwrap_or(&v);
    let drv = root
        .as_object()
        .and_then(|m| m.values().next())
        .ok_or("empty `nix derivation show` output")?;
    let env = drv.get("env").and_then(|e| e.as_object());
    let mut map = HashMap::new();
    if let Some(outputs) = drv.get("outputs").and_then(|o| o.as_object()) {
        for (name, spec) in outputs {
            // Input-addressed outputs carry `path`; fixed-output ones omit it
            // (only hash+method), but the path is mirrored in the `env` var of
            // the same name.
            let path = spec
                .get("path")
                .and_then(|p| p.as_str())
                .or_else(|| env.and_then(|e| e.get(name)).and_then(|v| v.as_str()));
            if let Some(path) = path {
                map.insert(name.clone(), path.to_string());
            }
        }
    }
    Ok(map)
}

/// Add a source file or directory to the store, with an explicit name, and
/// return its `/nix/store/...` path. Mirrors `nix-store --add` semantics.
pub fn add_source(path: &str) -> Result<String, String> {
    let out = Command::new("nix-store")
        .args(["--add", path])
        .output()
        .map_err(|e| format!("spawn `nix-store --add`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`nix-store --add {path}` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
