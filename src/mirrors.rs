//! Deterministic URL handling for `builtin:download`.
//!
//! Guix records either a quoted URL or a Scheme list of fallback URLs.  Its
//! download derivations also carry a `mirrors` input source: the serialized
//! `%mirrors` table used by Guix's download builder.  We parse that exact table
//! during translation, expand every matching `mirror://` entry in table order,
//! and stable-de-duplicate.  Emitted expressions contain only concrete URLs,
//! never a reference to the Guix store input.  Availability is deliberately not
//! considered here.

use std::collections::BTreeMap;
use std::fs;

pub type MirrorTable = BTreeMap<String, Vec<String>>;

/// Extract every double-quoted token from a Guix `url` env value.
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

#[derive(Debug, PartialEq, Eq)]
enum Sexp {
    Atom(String),
    String(String),
    List(Vec<Sexp>),
}

fn parse_sexp(input: &str) -> Result<Sexp, String> {
    fn skip_ws(bytes: &[u8], pos: &mut usize) {
        while *pos < bytes.len() && bytes[*pos].is_ascii_whitespace() {
            *pos += 1;
        }
    }
    fn parse_one(bytes: &[u8], pos: &mut usize) -> Result<Sexp, String> {
        skip_ws(bytes, pos);
        match bytes.get(*pos) {
            Some(b'(') => {
                *pos += 1;
                let mut items = Vec::new();
                loop {
                    skip_ws(bytes, pos);
                    match bytes.get(*pos) {
                        Some(b')') => {
                            *pos += 1;
                            return Ok(Sexp::List(items));
                        }
                        Some(_) => items.push(parse_one(bytes, pos)?),
                        None => return Err("unterminated mirror table list".into()),
                    }
                }
            }
            Some(b'"') => {
                *pos += 1;
                let mut value = String::new();
                loop {
                    match bytes.get(*pos) {
                        Some(b'"') => {
                            *pos += 1;
                            return Ok(Sexp::String(value));
                        }
                        Some(b'\\') => {
                            *pos += 1;
                            let escaped = *bytes
                                .get(*pos)
                                .ok_or("unterminated escape in mirror table")?;
                            value.push(escaped as char);
                            *pos += 1;
                        }
                        Some(byte) => {
                            value.push(*byte as char);
                            *pos += 1;
                        }
                        None => return Err("unterminated mirror table string".into()),
                    }
                }
            }
            Some(_) => {
                let start = *pos;
                while *pos < bytes.len()
                    && !bytes[*pos].is_ascii_whitespace()
                    && !matches!(bytes[*pos], b'(' | b')')
                {
                    *pos += 1;
                }
                Ok(Sexp::Atom(
                    std::str::from_utf8(&bytes[start..*pos])
                        .map_err(|_| "non-UTF-8 mirror table")?
                        .to_string(),
                ))
            }
            None => Err("empty mirror table".into()),
        }
    }

    let bytes = input.as_bytes();
    let mut pos = 0;
    let value = parse_one(bytes, &mut pos)?;
    skip_ws(bytes, &mut pos);
    if pos != bytes.len() {
        return Err("trailing data in mirror table".into());
    }
    Ok(value)
}

/// Parse Guix's serialized `%mirrors` input source.
pub fn parse_mirror_table(raw: &str) -> Result<MirrorTable, String> {
    let Sexp::List(entries) = parse_sexp(raw)? else {
        return Err("mirror table is not a list".into());
    };
    let mut table = MirrorTable::new();
    for entry in entries {
        let Sexp::List(mut fields) = entry else {
            return Err("mirror table entry is not a list".into());
        };
        if fields.is_empty() {
            return Err("mirror table has an empty entry".into());
        }
        let scheme = match fields.remove(0) {
            Sexp::Atom(s) | Sexp::String(s) => s,
            Sexp::List(_) => return Err("mirror table scheme is not an atom".into()),
        };
        let mut bases = Vec::with_capacity(fields.len());
        for field in fields {
            match field {
                Sexp::String(base) => bases.push(base),
                _ => return Err(format!("mirror table {scheme:?} has a non-string URL")),
            }
        }
        if table.insert(scheme.clone(), bases).is_some() {
            return Err(format!("mirror table repeats scheme {scheme:?}"));
        }
    }
    Ok(table)
}

/// Load the Guix-provided serialized `%mirrors` table from a download
/// derivation's `mirrors` input source.
pub fn load_mirror_table(path: &str) -> Result<MirrorTable, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("read mirrors input {path}: {e}"))?;
    parse_mirror_table(&raw).map_err(|e| format!("parse mirrors input {path}: {e}"))
}

/// Build concrete upstream candidates in Guix declaration order.
///
/// A `mirror://` declaration expands to every base in its matching Guix table
/// entry, retaining that entry's order.  Scheme matching is longest-prefix, so
/// `mirror://gnu/alpha/foo` selects a `gnu/alpha` entry when present and falls
/// back to `gnu` with path `alpha/foo` otherwise.  Unknown schemes are omitted
/// because they are not usable without Guix's mirror machinery.  Duplicate
/// concrete URLs retain their first position.
pub fn candidate_urls(urls: &[String], mirrors: &MirrorTable) -> Vec<String> {
    let mut candidates = Vec::new();
    for url in urls {
        let expanded = if url.starts_with("mirror://") {
            expand_mirror(url, mirrors).unwrap_or_default()
        } else {
            vec![url.clone()]
        };
        for candidate in expanded {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

/// Prepend an optional CA mirror and stable-de-duplicate the complete list.
pub fn ordered_candidates(
    ca_mirror: Option<String>,
    upstream: &[String],
    mirrors: &MirrorTable,
) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(url) = ca_mirror {
        candidates.push(url);
    }
    for url in candidate_urls(upstream, mirrors) {
        if !candidates.contains(&url) {
            candidates.push(url);
        }
    }
    candidates
}

/// Expand a `mirror://scheme/path` URL with every matching Guix mirror base.
pub fn expand_mirror(url: &str, mirrors: &MirrorTable) -> Option<Vec<String>> {
    let rest = url.strip_prefix("mirror://")?;
    let (scheme, path) = mirrors
        .keys()
        .filter_map(|scheme| {
            rest.strip_prefix(scheme)
                .and_then(|suffix| suffix.strip_prefix('/'))
                .map(|path| (scheme, path))
        })
        .max_by_key(|(scheme, _)| scheme.len())?;
    Some(
        mirrors[scheme]
            .iter()
            .map(|base| format!("{}/{}", base.trim_end_matches('/'), path.trim_matches('/')))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> MirrorTable {
        parse_mirror_table(
            r#"((gnu "https://gnu-one/" "https://gnu-two/")
                (gnu/alpha "https://alpha-one/" "https://alpha-two/")
                (sourceforge "https://sf-one/project/" "https://sf-two/project/")
                (mate "https://mate-one/releases/" "https://mate-two/releases/"))"#,
        )
        .unwrap()
    }

    #[test]
    fn extract_single_and_list() {
        assert_eq!(
            extract_urls("\"https://a/b.tar.gz\""),
            vec!["https://a/b.tar.gz"]
        );
        assert_eq!(
            extract_urls("(\"https://a/x\" \"https://b/x\" \"ftp://c/x\")"),
            vec!["https://a/x", "https://b/x", "ftp://c/x"]
        );
    }

    #[test]
    fn parses_guix_mirror_file_format() {
        let mirrors =
            parse_mirror_table(r#"((gnu "https://gnu/") (mate "https://mate/"))"#).unwrap();
        assert_eq!(mirrors["gnu"], ["https://gnu/"]);
        assert_eq!(mirrors["mate"], ["https://mate/"]);
    }

    #[test]
    fn expands_complete_ordered_lists_including_longest_prefix_and_omitted_schemes() {
        let urls = vec![
            "mirror://gnu/hello/h.tar.gz".to_string(),
            "mirror://gnu/alpha/guile/a.tar.gz".to_string(),
            "mirror://sourceforge/foo/foo-1.tar.gz".to_string(),
            "mirror://mate/core/mate-1.tar.gz".to_string(),
        ];
        assert_eq!(
            candidate_urls(&urls, &table()),
            vec![
                "https://gnu-one/hello/h.tar.gz",
                "https://gnu-two/hello/h.tar.gz",
                "https://alpha-one/guile/a.tar.gz",
                "https://alpha-two/guile/a.tar.gz",
                "https://sf-one/project/foo/foo-1.tar.gz",
                "https://sf-two/project/foo/foo-1.tar.gz",
                "https://mate-one/releases/core/mate-1.tar.gz",
                "https://mate-two/releases/core/mate-1.tar.gz",
            ]
        );
    }

    #[test]
    fn falls_back_to_shorter_scheme_when_table_has_no_longer_one() {
        let mirrors = parse_mirror_table(r#"((gnu "https://gnu/"))"#).unwrap();
        assert_eq!(
            expand_mirror("mirror://gnu/alpha/foo", &mirrors),
            Some(vec!["https://gnu/alpha/foo".into()])
        );
    }

    #[test]
    fn candidates_preserve_declaration_order_after_expansion_and_dedup() {
        let urls = vec![
            "https://first/x".to_string(),
            "mirror://gnu/hello/h.tar.gz".to_string(),
            "https://gnu-two/hello/h.tar.gz".to_string(),
            "mirror://unknown/x".to_string(),
            "https://third/x".to_string(),
        ];
        assert_eq!(
            candidate_urls(&urls, &table()),
            vec![
                "https://first/x",
                "https://gnu-one/hello/h.tar.gz",
                "https://gnu-two/hello/h.tar.gz",
                "https://third/x",
            ]
        );
    }

    #[test]
    fn normalizes_mirror_base_and_path_slashes_without_changing_order_or_dedup() {
        let mirrors = parse_mirror_table(
            r#"((imagemagick "https://one.example/releases"
                            "https://two.example/releases/"
                            "https://one.example/releases/"))"#,
        )
        .unwrap();
        let urls = vec![
            "https://first.example/source".to_string(),
            "mirror://imagemagick//ImageMagick-7.1.1.tar.xz/".to_string(),
            "https://two.example/releases/ImageMagick-7.1.1.tar.xz".to_string(),
            "https://last.example/source".to_string(),
        ];

        assert_eq!(
            candidate_urls(&urls, &mirrors),
            vec![
                "https://first.example/source",
                "https://one.example/releases/ImageMagick-7.1.1.tar.xz",
                "https://two.example/releases/ImageMagick-7.1.1.tar.xz",
                "https://last.example/source",
            ]
        );
    }

    #[test]
    fn default_and_upstream_policy_are_deterministic() {
        let upstream = vec![
            "https://first/x".to_string(),
            "mirror://gnu/hello/h.tar.gz".to_string(),
        ];
        assert_eq!(
            ordered_candidates(Some("https://ca/x".into()), &upstream, &table()),
            vec![
                "https://ca/x",
                "https://first/x",
                "https://gnu-one/hello/h.tar.gz",
                "https://gnu-two/hello/h.tar.gz",
            ]
        );
        assert_eq!(
            ordered_candidates(None, &upstream, &table()),
            vec![
                "https://first/x",
                "https://gnu-one/hello/h.tar.gz",
                "https://gnu-two/hello/h.tar.gz",
            ]
        );
    }
}
