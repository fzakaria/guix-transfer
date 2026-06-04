//! Serialize a (already path-translated) [`Derivation`] into the JSON format
//! that `nix derivation add` consumes (format version 4).
//!
//! Output paths are intentionally emitted empty so the Nix daemon computes
//! them via `hashDerivationModulo`; for fixed-output derivations we instead
//! emit the SRI hash + method and let Nix derive the path from that.

use crate::ast::{Derivation, derivation_name};
use crate::hash;
use serde_json::{Map, Value, json};

/// Store-relative base name: the part after the last `/`.
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build the JSON value for `nix derivation add`.
///
/// `drv_path` is the *Guix* `.drv` path, used only to recover the derivation
/// name. The derivation itself must already have all store paths rewritten to
/// `/nix/store` and its own output-path env vars blanked.
pub fn to_nix_json(drv: &Derivation, drv_path: &str) -> Result<Value, String> {
    let name = derivation_name(drv_path);

    // In JSON v4, input drv keys and src entries are *store-relative* base
    // names (no `/nix/store/` prefix), whereas env/args/builder keep full paths.
    let mut drvs = Map::new();
    for input in &drv.input_drvs {
        drvs.insert(
            base_name(&input.path).to_string(),
            json!({ "outputs": input.outputs, "dynamicOutputs": {} }),
        );
    }
    let srcs: Vec<String> = drv
        .input_srcs
        .iter()
        .map(|s| base_name(s).to_string())
        .collect();

    let mut env = Map::new();
    for e in &drv.env {
        env.insert(e.key.clone(), Value::String(e.value.clone()));
    }

    let executable = drv.env_get("executable") == Some("1");
    let mut outputs = Map::new();
    for out in &drv.outputs {
        if out.hash.is_empty() {
            // Input-addressed: empty object, Nix computes the path.
            outputs.insert(out.name.clone(), json!({}));
        } else {
            let h = hash::guix_to_nix(&out.hash_algo, &out.hash, executable)?;
            outputs.insert(
                out.name.clone(),
                json!({ "hash": h.sri, "method": h.method }),
            );
        }
    }

    Ok(json!({
        "version": 4,
        "name": name,
        "system": drv.system,
        "builder": drv.builder,
        "args": drv.args,
        "env": Value::Object(env),
        "inputs": { "drvs": Value::Object(drvs), "srcs": srcs },
        "outputs": Value::Object(outputs),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{EnvVar, Output};

    #[test]
    fn minimal_json_shape() {
        let drv = Derivation {
            outputs: vec![Output {
                name: "out".into(),
                path: String::new(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![],
            input_srcs: vec![],
            system: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: vec!["-c".into(), "echo hi > $out".into()],
            env: vec![EnvVar {
                key: "out".into(),
                value: String::new(),
            }],
        };
        let v = to_nix_json(
            &drv,
            "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-minimal.drv",
        )
        .unwrap();
        assert_eq!(v["version"], 4);
        assert_eq!(v["name"], "minimal");
        assert_eq!(v["outputs"]["out"], json!({}));
        assert_eq!(v["inputs"]["drvs"], json!({}));
    }

    #[test]
    fn fod_json_has_sri_and_method() {
        let drv = Derivation {
            outputs: vec![Output {
                name: "out".into(),
                path: String::new(),
                hash_algo: "sha256".into(),
                hash: "cf04afc05f242978a9d86171195aa04332993ba89f81d11b3273913000cc649c".into(),
            }],
            input_drvs: vec![],
            input_srcs: vec![],
            system: "builtin".into(),
            builder: "builtin:fetchurl".into(),
            args: vec![],
            env: vec![
                EnvVar {
                    key: "out".into(),
                    value: String::new(),
                },
                EnvVar {
                    key: "url".into(),
                    value: "https://x/y.tar.gz".into(),
                },
            ],
        };
        let v = to_nix_json(
            &drv,
            "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-source.drv",
        )
        .unwrap();
        assert_eq!(
            v["outputs"]["out"]["hash"],
            "sha256-zwSvwF8kKXip2GFxGVqgQzKZO6ifgdEbMnORMADMZJw="
        );
        assert_eq!(v["outputs"]["out"]["method"], "flat");
        assert_eq!(v["builder"], "builtin:fetchurl");
    }
}
