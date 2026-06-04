//! URL handling for `builtin:download` → `builtin:fetchurl`.
//!
//! Guix's `url` env var holds a Scheme value: either a single quoted string
//! `"\"u\""` or a list of mirror fallbacks `("u1" "u2" ...)`. Nix's
//! `builtin:fetchurl` accepts exactly one URL, so we extract every candidate
//! and pick the best one, expanding `mirror://scheme/...` against a ported
//! subset of Guix's mirror table.

/// Extract every double-quoted token from a Guix `url` env value.
///
/// Works for both `"\"u\""` (already unescaped to `"u"`) and `(... )` lists.
/// If no quotes are present the whole trimmed string is treated as one URL.
pub fn extract_urls(raw: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut saw_quote = false;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            saw_quote = true;
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                j += 1;
            }
            urls.push(raw[start..j].to_string());
            i = j + 1;
        } else {
            i += 1;
        }
    }
    if !saw_quote {
        let trimmed = raw.trim().trim_matches(['(', ')']).trim();
        if !trimmed.is_empty() {
            urls.push(trimmed.to_string());
        }
    }
    urls
}

/// Ordered list of concrete URLs to try, best first.
///
/// `builtin:fetchurl` cannot fall back across a mirror list, so the splicer
/// probes these in order and keeps the first that responds. We expand every
/// `mirror://` entry we understand, drop unknown mirror schemes, de-duplicate,
/// and rank by host reliability — canonical project mirrors beat ad-hoc
/// personal mirrors (e.g. `lilypond.org/janneke`, which 404s for older
/// bootstrap tarballs). Note even "good" hosts can 404 a given file (bootstrap
/// binaries live only on `alpha.gnu.org`), which is why probing matters.
pub fn candidate_urls(urls: &[String]) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    for u in urls {
        if let Some(expanded) = expand_mirror(u) {
            candidates.push(expanded);
        } else if !u.starts_with("mirror://") {
            candidates.push(u.clone());
        }
    }
    if candidates.is_empty() {
        candidates = urls.to_vec();
    }
    // Stable sort by descending score keeps original order among equal hosts.
    candidates.sort_by_key(|u| -host_score(u));
    candidates.dedup();
    candidates
}

/// Higher is better. Canonical, long-lived mirrors rank highest; known-flaky
/// personal mirrors rank lowest.
fn host_score(url: &str) -> i32 {
    const GOOD: &[&str] = &[
        "ftp.gnu.org",
        "ftpmirror.gnu.org",
        "download.savannah.nongnu.org",
        "savannah.gnu.org",
        "downloads.sourceforge.net",
        "www.kernel.org",
        "download.gnome.org",
        "download.kde.org",
        "files.pythonhosted.org",
        "github.com",
        "gnupg.org",
    ];
    const BAD: &[&str] = &["lilypond.org", "flashner.co.il", "fdn.fr", "www.fdn.fr"];
    if GOOD.iter().any(|h| url.contains(h)) {
        2
    } else if BAD.iter().any(|h| url.contains(h)) {
        -1
    } else {
        1
    }
}

/// Expand a `mirror://scheme/path` URL using the first mirror of `scheme`.
/// Returns `None` if `url` is not a mirror URL or the scheme is unknown.
pub fn expand_mirror(url: &str) -> Option<String> {
    let rest = url.strip_prefix("mirror://")?;
    let (scheme, path) = rest.split_once('/')?;
    let base = mirror_base(scheme)?;
    Some(format!("{base}{path}"))
}

/// First mirror base URL for a Guix mirror scheme (subset of `guix/download.scm`).
fn mirror_base(scheme: &str) -> Option<&'static str> {
    Some(match scheme {
        "gnu" | "gnu/alpha" => "https://ftp.gnu.org/gnu/",
        "savannah" => "https://download.savannah.nongnu.org/releases/",
        "sourceforge" => "https://downloads.sourceforge.net/",
        "kernel.org" => "https://www.kernel.org/pub/",
        "apache" => "https://dlcdn.apache.org/",
        "gnome" => "https://download.gnome.org/",
        "kde" => "https://download.kde.org/",
        "xorg" => "https://www.x.org/releases/",
        "gnupg" => "https://gnupg.org/ftp/gcrypt/",
        "cpan" => "https://www.cpan.org/",
        "pypi" => "https://files.pythonhosted.org/packages/",
        "bioconductor" => "https://bioconductor.org/",
        "github" => "https://github.com/",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_single() {
        assert_eq!(extract_urls("\"https://a/b.tar.gz\""), vec!["https://a/b.tar.gz"]);
    }

    #[test]
    fn extract_list() {
        let raw = "(\"https://a/x\" \"https://b/x\" \"ftp://c/x\")";
        assert_eq!(extract_urls(raw), vec!["https://a/x", "https://b/x", "ftp://c/x"]);
    }

    #[test]
    fn extract_unquoted() {
        assert_eq!(extract_urls("mirror://gnu/hello/h.tar"), vec!["mirror://gnu/hello/h.tar"]);
    }

    #[test]
    fn prefer_canonical_mirror_over_random_host() {
        let urls = vec![
            "mirror://savannah/x.tar".to_string(),
            "https://random/x.tar".to_string(),
        ];
        // Canonical savannah mirror outranks an unknown host.
        assert_eq!(
            candidate_urls(&urls)[0],
            "https://download.savannah.nongnu.org/releases/x.tar"
        );
    }

    #[test]
    fn mes_picks_gnu_over_lilypond() {
        // Regression: builtin:fetchurl can't fall back, and lilypond.org 404s.
        let urls = vec![
            "mirror://gnu/mes/mes-0.25.1.tar.gz".to_string(),
            "https://lilypond.org/janneke/mes/mes-0.25.1.tar.gz".to_string(),
        ];
        assert_eq!(
            candidate_urls(&urls)[0],
            "https://ftp.gnu.org/gnu/mes/mes-0.25.1.tar.gz"
        );
    }

    #[test]
    fn expands_known_mirror() {
        assert_eq!(
            expand_mirror("mirror://gnu/hello/hello-2.12.tar.gz").unwrap(),
            "https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz"
        );
        assert_eq!(
            expand_mirror("mirror://savannah/tinycc/tcc-0.9.27.tar.bz2").unwrap(),
            "https://download.savannah.nongnu.org/releases/tinycc/tcc-0.9.27.tar.bz2"
        );
    }

    #[test]
    fn unknown_mirror_is_none() {
        assert!(expand_mirror("mirror://nope/x").is_none());
        assert!(expand_mirror("https://x/y").is_none());
    }

    #[test]
    fn pick_expands_when_only_mirror() {
        let urls = vec!["mirror://gnu/hello/h.tar.gz".to_string()];
        assert_eq!(candidate_urls(&urls)[0], "https://ftp.gnu.org/gnu/hello/h.tar.gz");
    }
}
