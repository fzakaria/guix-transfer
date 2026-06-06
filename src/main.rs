mod ast;
mod emit_nix;
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
use std::path::Path;

fn main() -> Result<(), String> {
    let mut verbose = false;
    let mut upstream = false;
    let mut emit_nix_path: Option<String> = None;
    let mut root_drv = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--upstream" => upstream = true,
            "--emit-nix" => {
                emit_nix_path = Some(
                    args.next()
                        .ok_or("--emit-nix requires an output path argument")?,
                );
            }
            _ => root_drv = Some(arg),
        }
    }
    let Some(root_drv) = root_drv else {
        eprintln!("Usage: guix-transfer [-v] [--upstream] [--emit-nix <output.nix>] <guix_drv_file>");
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
    println!("{final_drv}");
    eprintln!("Realise it with: nix-store --realise --option filter-syscalls false {final_drv}");

    if let Some(nix_path) = emit_nix_path {
        emit_nix::emit(Path::new(&nix_path), &splicer.translated)?;
        eprintln!("Emitted Nix expression: {nix_path}");
    }

    Ok(())
}
