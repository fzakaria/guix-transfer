mod ast;
mod parser;
mod graph;
mod splicer;

use std::env;
use std::process::Command;
use crate::graph::DerivationGraph;
use crate::splicer::Splicer;

fn get_nix_stdenv() -> Result<String, String> {
    let output = Command::new("nix-instantiate")
        .args(&["<nixpkgs>", "-A", "stdenv.cc"])
        .output()
        .map_err(|e| format!("Failed to run nix-instantiate: {}", e))?;

    if !output.status.success() {
        return Err(format!("nix-instantiate failed: {}", String::from_utf8_lossy(&output.stderr)));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn main() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: {} <guix_drv_file>", args[0]);
        return Ok(());
    }

    let root_drv = &args[1];
    println!("Loading Guix derivation graph from {}...", root_drv);

    let mut graph = DerivationGraph::new();
    graph.load_recursive(root_drv)?;

    println!("Loaded {} derivations.", graph.derivations.len());

    println!("Getting Nix stdenv...");
    let nix_stdenv = get_nix_stdenv()?;
    println!("Nix stdenv: {}", nix_stdenv);

    println!("Splicing and translating...");
    let mut splicer = Splicer::new(nix_stdenv);
    let final_drv = splicer.run(&graph)?;

    println!("Successfully translated to Nix!");
    println!("Final Nix derivation: {}", final_drv);
    println!("You can now run: nix-store --realise {}", final_drv);

    Ok(())
}
