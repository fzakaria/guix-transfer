//! Shared rendering and metadata for translated `builtin:download` sources.
//!
//! Both direct translation and Nix emitters use this exact pinned-nixpkgs
//! `fetchurl` helper.  Its `urls` argument keeps fallback at build time rather
//! than making translation depend on reachability.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchUrlSource {
    pub urls: Vec<String>,
    pub name: String,
    pub hash_sri: String,
    pub recursive_hash: bool,
    pub executable: bool,
    /// Guix download target system.  Concrete values select the matching
    /// nixpkgs legacyPackages set; `builtin` is normalized by the helper.
    pub system: String,
    pub drv_path: String,
    pub out_path: String,
}

/// The pinned helper used verbatim for direct instantiation and emitted files.
pub fn helper(nixpkgs_rev: &str) -> String {
    let flake = crate::splicer::nixpkgs_flake_ref(nixpkgs_rev);
    format!(
        "{{ urls, hash, name, system, recursiveHash ? false, executable ? false }}:\n\
         let targetSystem = if system == \"builtin\" then builtins.currentSystem else system; in\n\
         (builtins.getFlake {flake:?}).legacyPackages.${{targetSystem}}.fetchurl {{\n\
         \x20 inherit urls hash name recursiveHash executable;\n\
         }}"
    )
}

/// A helper invocation. `fetchurl` must be bound to [`helper`]'s result.
pub fn call(source: &FetchUrlSource) -> String {
    let urls = source
        .urls
        .iter()
        .map(|url| crate::ast::nix_string_literal(url))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "fetchurl {{ urls = [ {urls} ]; hash = {hash}; name = {name}; system = {system}; recursiveHash = {recursive}; executable = {executable}; }}",
        hash = crate::ast::nix_string_literal(&source.hash_sri),
        name = crate::ast::nix_string_literal(&source.name),
        system = crate::ast::nix_string_literal(&source.system),
        recursive = source.recursive_hash,
        executable = source.executable,
    )
}

/// A standalone expression suitable for `nix eval`; it uses the same helper
/// body that emitted expressions write to `fetch-url.nix`.
pub fn expression(nixpkgs_rev: &str, source: &FetchUrlSource) -> String {
    format!(
        "let fetchurl = {}; in {}",
        helper(nixpkgs_rev),
        call(source)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> FetchUrlSource {
        FetchUrlSource {
            urls: vec![
                "https://dead.invalid/a".into(),
                "https://good.invalid/a".into(),
            ],
            name: "fixture".into(),
            hash_sri: "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            recursive_hash: true,
            executable: true,
            system: "aarch64-linux".into(),
            drv_path: String::new(),
            out_path: String::new(),
        }
    }

    #[test]
    fn helper_call_embeds_ordered_urls_and_mode() {
        let call = call(&source());
        assert!(call.contains("urls = [ \"https://dead.invalid/a\" \"https://good.invalid/a\" ]"));
        assert!(call.contains("system = \"aarch64-linux\""));
        assert!(call.contains("recursiveHash = true"));
        assert!(call.contains("executable = true"));
    }

    #[test]
    fn expression_uses_pinned_helper() {
        let expr = expression("abc", &source());
        assert!(expr.contains("github:NixOS/nixpkgs/abc"));
        assert!(expr.contains("legacyPackages.${targetSystem}.fetchurl"));
        assert!(!expr.contains("legacyPackages.x86_64-linux.fetchurl"));
        assert!(expr.contains("fetchurl { urls ="));
    }

    #[test]
    fn builtin_system_uses_the_evaluators_system() {
        let mut builtin = source();
        builtin.system = "builtin".into();
        let expr = expression("abc", &builtin);
        assert!(expr.contains("if system == \"builtin\" then builtins.currentSystem else system"));
        assert!(call(&builtin).contains("system = \"builtin\""));
    }

    #[test]
    fn call_escapes_nix_interpolation_quotes_and_backslashes() {
        let mut source = source();
        source.urls = vec![r#"https://example.invalid/a\b/"quoted"/${not_nix}"#.into()];

        assert!(
            call(&source)
                .contains(r#"urls = [ "https://example.invalid/a\\b/\"quoted\"/\${not_nix}" ]"#)
        );
    }
}
