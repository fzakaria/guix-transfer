//! Hash format conversion.
//!
//! Guix records fixed-output hashes as lowercase base16 (hex), with the algo
//! field either `"sha256"` (flat) or `"r:sha256"` (recursive / NAR). Nix's
//! JSON derivation format (v4) wants the hash in SRI form
//! (`sha256-<base64>`) plus a separate `method` of `"flat"` or `"nar"`.
//!
//! Everything here is pure so it can be unit-tested without a store.

/// A fixed-output hash translated into the shape Nix's JSON format wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NixHash {
    /// SRI string, e.g. `sha256-zwSv...`.
    pub sri: String,
    /// `"flat"` or `"nar"`.
    pub method: String,
}

/// Convert a Guix `(hash_algo, hash)` pair into a [`NixHash`].
///
/// `executable` reflects the Guix `executable=1` download flag, which (like
/// `r:` algos) implies a recursive/NAR hash.
pub fn guix_to_nix(hash_algo: &str, hash_hex: &str, executable: bool) -> Result<NixHash, String> {
    let recursive = hash_algo.starts_with("r:") || executable;
    let bare_algo = hash_algo.trim_start_matches("r:");
    if bare_algo != "sha256" {
        return Err(format!("unsupported hash algo {hash_algo:?} (only sha256)"));
    }
    let bytes = hex_decode(hash_hex)?;
    if bytes.len() != 32 {
        return Err(format!(
            "sha256 hash must be 32 bytes, got {} from {hash_hex:?}",
            bytes.len()
        ));
    }
    Ok(NixHash {
        sri: format!("sha256-{}", base64_encode(&bytes)),
        method: if recursive { "nar" } else { "flat" }.to_string(),
    })
}

/// Decode a lowercase/uppercase base16 string into bytes.
pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("hex string has odd length: {s:?}"));
    }
    let nibble = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("invalid hex char {:?}", c as char)),
        }
    };
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Ok(out)
}

/// Nix/Guix base32 alphabet (omits e, o, u, t).
const NIX_B32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Encode bytes in Nix/Guix base32 (used by content-addressed store paths and
/// the Guix CA-mirror URL scheme). Same algorithm as `nix hash --to nix32`.
pub fn nix_base32(data: &[u8]) -> String {
    let len = (data.len() * 8).div_ceil(5);
    let mut out = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let mut c = (data[i] as u32) >> j;
        if i + 1 < data.len() {
            c |= (data[i + 1] as u32) << (8 - j);
        }
        out.push(NIX_B32[(c & 0x1f) as usize] as char);
    }
    out
}

/// Build a Guix content-addressed-mirror URL for a sha256 (hex) hash.
/// `name` is the source file name (e.g. `hello-2.12.tar.gz`). The mirror serves
/// any source Guix's CI has seen, keyed purely by content hash, so it is far
/// more reliable than the upstream mirror list (which `builtin:fetchurl` cannot
/// fall back across).
pub fn guix_ca_mirror_url(name: &str, hash_hex: &str) -> Result<String, String> {
    let bytes = hex_decode(hash_hex.trim_start_matches("r:"))?;
    Ok(format!(
        "https://bordeaux.guix.gnu.org/file/{name}/sha256/{}",
        nix_base32(&bytes)
    ))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648) with `=` padding — matches SRI encoding.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_decode("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(hex_decode("0").is_err());
        assert!(hex_decode("zz").is_err());
    }

    #[test]
    fn sri_matches_nix() {
        // `nix hash convert --to sri` of this hex gives this SRI.
        let h = guix_to_nix(
            "sha256",
            "cf04afc05f242978a9d86171195aa04332993ba89f81d11b3273913000cc649c",
            false,
        )
        .unwrap();
        assert_eq!(h.sri, "sha256-zwSvwF8kKXip2GFxGVqgQzKZO6ifgdEbMnORMADMZJw=");
        assert_eq!(h.method, "flat");
    }

    #[test]
    fn recursive_via_algo_or_executable() {
        let hex = "6f887d45fa0f7e59e55c6d7ba86a3d8c35369c7afbb3a5829b8ed226bfef4a66";
        assert_eq!(guix_to_nix("r:sha256", hex, false).unwrap().method, "nar");
        assert_eq!(guix_to_nix("sha256", hex, true).unwrap().method, "nar");
        assert_eq!(guix_to_nix("sha256", hex, false).unwrap().method, "flat");
    }

    #[test]
    fn rejects_non_sha256() {
        assert!(guix_to_nix("sha1", "00", false).is_err());
    }

    #[test]
    fn nix_base32_matches_nix() {
        // hex → nix32, verified against `nix hash convert --to nix32`.
        let cases = [
            (
                "ba621bff6adc2e9e381f5907e0e86ad22b191678404e1f2888a5a924fa02031d",
                "07830bx29ad5i0l1ykj0g0b1jayjdblf01sr3ww9wbnwdbzinqms",
            ),
            (
                "037b103522a2d0d7d69c7ffd8de683dfe5bb4b59c1fafd70b4ffd397fd2f57f0",
                "1w2p5zyrglzzniqgvyn1b55vprfzhgk8vzbzkkbdgl5248si0yq3",
            ),
        ];
        for (hex, want) in cases {
            assert_eq!(nix_base32(&hex_decode(hex).unwrap()), want, "hex={hex}");
        }
    }

    #[test]
    fn ca_mirror_url_handles_recursive_prefix() {
        // The `r:` algo prefix must not leak into the hash bytes.
        let u = guix_ca_mirror_url(
            "tar",
            "ba621bff6adc2e9e381f5907e0e86ad22b191678404e1f2888a5a924fa02031d",
        )
        .unwrap();
        assert_eq!(
            u,
            "https://bordeaux.guix.gnu.org/file/tar/sha256/07830bx29ad5i0l1ykj0g0b1jayjdblf01sr3ww9wbnwdbzinqms"
        );
    }
}
