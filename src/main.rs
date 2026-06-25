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
    let mut disable_tests = false;
    let mut emit_nix_path: Option<String> = None;
    let mut emit_nix_dir: Option<String> = None;
    let mut root_drvs = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--upstream" => upstream = true,
            // Rewrite `#:tests? #t` → `#:tests? #f` in every builder during
            // translation, before paths are hashed, so the disabled-tests build
            // and all downstream references stay consistent. (Overlays can't do
            // this: builders bake in absolute dependency store paths.)
            "--disable-tests" => disable_tests = true,
            "--emit-nix" => {
                emit_nix_path = Some(
                    args.next()
                        .ok_or("--emit-nix requires an output path argument")?,
                );
            }
            "--emit-nix-dir" => {
                emit_nix_dir = Some(
                    args.next()
                        .ok_or("--emit-nix-dir requires an output directory argument")?,
                );
            }
            _ => root_drvs.push(arg),
        }
    }
    if root_drvs.is_empty() {
        eprintln!(
            "Usage: guix-transfer [-v] [--upstream] [--disable-tests] [--emit-nix <output.nix>] [--emit-nix-dir <output_dir>] <guix_drv_file>..."
        );
        return Err("missing derivation argument".into());
    };

    eprintln!("Loading Guix derivation graphs ...");
    let mut graph = DerivationGraph::new();
    graph.load_recursive_multi(&root_drvs)?;
    eprintln!("Loaded {} derivations.", graph.derivations.len());

    eprintln!("Translating bottom-up ...");
    let mut splicer = Splicer::new();
    splicer.verbose = verbose;
    splicer.upstream = upstream;
    splicer.disable_tests = disable_tests;
    let _final_drv = splicer.run(&graph)?;

    eprintln!("Done. Final Nix derivations:");
    for root_drv in &root_drvs {
        if let Some(nix_drv) = splicer.map.get(root_drv) {
            println!("{}", nix_drv.value());
        }
    }

    if let Some(nix_path) = emit_nix_path {
        emit_nix::emit(Path::new(&nix_path), &splicer.translated.lock().unwrap())?;
        eprintln!("Emitted Nix expression: {nix_path}");
    }

    if let Some(nix_dir) = emit_nix_dir {
        let map: std::collections::HashMap<String, String> = splicer
            .map
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();
        // Git sources keyed by their realized Nix store path (how emit_nix sees
        // them in input_srcs / builder strings).
        let git_sources: std::collections::HashMap<String, splicer::GitSource> = splicer
            .git_sources
            .iter()
            .map(|r| (r.value().nix_path.clone(), r.value().clone()))
            .collect();
        emit_nix::emit_dir(
            Path::new(&nix_dir),
            &splicer.translated.lock().unwrap(),
            &map,
            &git_sources,
        )?;
        eprintln!("Emitted multi-file Nix expressions into: {nix_dir}");

        // Guard against silently emitting a "split-brain" tree: every emitted
        // `.nix` must evaluate to the same `.drv` that `nix derivation add`
        // produced (and whose output paths were baked into consumer builders).
        eprintln!("Verifying emitted store is internally consistent ...");
        emit_nix::verify_consistency(Path::new(&nix_dir), &splicer.translated.lock().unwrap())?;
        eprintln!("Consistency check passed: baked dependency paths match the emitted .nix paths.");
    }

    Ok(())
}
