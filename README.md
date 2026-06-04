# Guix-to-Nix Splicer 🏗️

A CLI tool for performing bottom-up translation of GNU Guix derivations into Nix.

## Goal
The goal of this project is to allow "organic" builds of Guix packages inside the Nix sandbox. It accomplishes this by:
1.  **Ingesting** a Guix `.drv` graph.
2.  **Splicing** the root toolchain: Identifying the Guix bootstrap boundary (e.g., `gcc-bootstrap`) and swapping it for the Nix `stdenv.cc`.
3.  **Rewriting Paths**: Translating all `/gnu/store` references in builder scripts, environment variables, and arguments into their corresponding `/nix/store` locations.
4.  **Bridging Builtins**: Delegating Guix's `builtin:download` to Nixpkgs' `fetchurl` to handle source fetching with multiple mirrors and fallbacks.
5.  **Converging Hashes**: Iteratively re-calculating Nix derivation paths until the internal `out` environment variables match Nix's strict hash validation.

## Usage

### 1. Enter a Nix shell with the required tools:
```bash
nix-shell -p cargo rustc gcc
```

### 2. Run the splicer on a Guix derivation:
```bash
cargo run -- /gnu/store/...-hello-2.12.2.drv
```

### 3. Realize the resulting Nix derivation:
```bash
nix-store --realise /nix/store/...-hello-2.12.2.drv
```