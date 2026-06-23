//! Minimal network probing, used to choose a working download URL before
//! committing it to a single-URL `builtin:fetchurl` derivation.

use std::sync::OnceLock;

static HTTP_AGENT: OnceLock<ureq::Agent> = OnceLock::new();

fn get_agent() -> &'static ureq::Agent {
    HTTP_AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(20)))
            .build()
            .into()
    })
}

/// Return true if `url` appears fetchable. Uses a tiny ranged GET via `ureq`
/// (more reliable than HEAD, which some mirrors reject) and accepts 200/206.
pub fn url_ok(url: &str) -> bool {
    let req = get_agent().get(url).header("Range", "bytes=0-0");

    match req.call() {
        Ok(res) => {
            let status = res.status();
            status == 200 || status == 206
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_ok() {
        let url = "https://bordeaux.guix.gnu.org/file/bash/sha256/0rjaxyzjdllfkf1abczvgaf3cdcc7mmahyvdbkjmjzhgz92pv23g";
        let req = get_agent().get(url).header("Range", "bytes=0-0");
        let res = req.call();
        println!("{:?}", res.map(|r| r.status()));
    }
}
