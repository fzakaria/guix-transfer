//! Minimal network probing, used to choose a working download URL before
//! committing it to a single-URL `builtin:fetchurl` derivation.

use std::process::Command;

/// Return true if `url` appears fetchable. Uses a tiny ranged GET via `curl`
/// (more reliable than HEAD, which some mirrors reject) and accepts 200/206.
pub fn url_ok(url: &str) -> bool {
    let out = Command::new("curl")
        .args([
            "-s",
            "-L",
            "-o",
            "/dev/null",
            "-r",
            "0-0",
            "--max-time",
            "20",
            "-w",
            "%{http_code}",
            url,
        ])
        .output();
    match out {
        Ok(o) => {
            let code = String::from_utf8_lossy(&o.stdout);
            let code = code.trim();
            code == "200" || code == "206"
        }
        Err(_) => false,
    }
}
