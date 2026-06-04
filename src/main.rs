mod ast;
mod graph;
mod hash;
mod json;
mod mirrors;
mod net;
mod nixstore;
mod parser;
mod splicer;

use crate::graph::DerivationGraph;
use crate::splicer::Splicer;
use std::env;

fn main() -> Result<(), String> {
    let mut verbose = false;
    let mut upstream = false;
    let mut root_drv = None;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            // Fetch download seeds from upstream mirrors (with probing) instead
            // of the Guix content-addressed mirror.
            "--upstream" => upstream = true,
            _ => root_drv = Some(arg),
        }
    }
    let Some(root_drv) = root_drv else {
        eprintln!("Usage: guix-transfer [-v] [--upstream] <guix_drv_file>");
        return Err("missing derivation argument".into());
    };

    eprintln!("Loading Guix derivation graph from {root_drv} ...");
    let mut graph = DerivationGraph::new();
    graph.load_recursive(&root_drv)?;
    eprintln!("Loaded {} derivations.", graph.derivations.len());

    eprintln!("Translating bottom-up ...");
    let mut splicer = Splicer::new();
    splicer.verbose = verbose;
    splicer.upstream = upstream;
    let final_drv = splicer.run(&graph)?;

    eprintln!("Done. Final Nix derivation:");
    // The drv path goes to stdout so it can be captured by scripts.
    println!("{final_drv}");
    eprintln!("Realise it with: nix-store --realise {final_drv}");
    Ok(())
}
