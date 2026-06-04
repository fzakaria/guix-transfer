# guix-transfer 🏗️

[![built with nix](https://builtwithnix.org/badge.svg)](https://builtwithnix.org)
[![Test](https://github.com/fzakaria/guix-transfer/actions/workflows/test.yml/badge.svg)](https://github.com/fzakaria/guix-transfer/actions/workflows/test.yml)

> Import a GNU Guix derivation graph into Nix and let `nix-daemon` build it — no
> rewriting to Nix expressions, no `stdenv`, no re-bootstrapping.

```console
❯ guix-transfer /gnu/store/w9krgvil6919s2ghqgx443zb9krx75s6-minimal.drv
Loading Guix derivation graph from /gnu/store/...-minimal.drv ...
Loaded 1 derivations.
Translating bottom-up ...
Done. Final Nix derivation:
/nix/store/m367ssr7zqj6mksp889gx4x177r2ngdi-minimal.drv

❯ nix-store --realise /nix/store/m367ssr7zqj6mksp889gx4x177r2ngdi-minimal.drv
/nix/store/c6dk6nhykapfl951rmvw22m99p1nzjwi-minimal

❯ cat /nix/store/c6dk6nhykapfl951rmvw22m99p1nzjwi-minimal
Success
```

## Why?

Guix and Nix feel like rival universes, but at the bottom they are the *same
thing*: a `.drv` is an [ATerm](https://en.wikipedia.org/wiki/ATerms)
`Derive(...)` record, and `nix-daemon` / `guix-daemon` are both just sandboxed
builders that take such a record and produce its outputs. A Guix derivation is
already fully hermetic — it lists every input, every source, every env var.

So do we really need to *port* a Guix package to build it under Nix? _No._ We
can translate the derivation graph directly and hand it to the Nix daemon. The
only differences are cosmetic: the store prefix (`/gnu/store` vs `/nix/store`),
how output paths are hashed (same algorithm, different store dir → different
paths), and the `builtin:download` vs `builtin:fetchurl` source fetcher.

The fun part: this goes _all the way down_. Guix's whole world is built from a
tiny set of statically-linked seed binaries it downloads. Those seeds have no
baked-in store paths, so once translated they run in the Nix sandbox unchanged,
and everything above them — `mes`, `tcc`, `gcc-mesboot`, `glibc`, `guile`,
`gcc`, … up to `hello` — builds *organically* in Nix from Guix's own sources.

> **Note:** this is a proof-of-concept / curiosity, not a packaging strategy.
> The resulting `/nix/store` paths are content-equivalent to Guix's, but built
> by the Nix daemon. Realising `hello` end-to-end recompiles Guix's entire
> source bootstrap, which takes hours.

## How it works

`guix-transfer` walks the `.drv` DAG in post-order and, for each derivation:

1. **`builtin:download` → `builtin:fetchurl`.** The URL is rewritten to Guix's
   content-addressed mirror, `https://bordeaux.guix.gnu.org/file/<name>/sha256/
   <hash>`. `builtin:fetchurl` can only take one URL and can't fall back, and
   the upstream mirror lists are flaky — but the CA mirror serves *any* source
   Guix's CI has seen, keyed by the hash we already have. One reliable URL.
2. **Sources are added** to the Nix store (text files get their `/gnu/store`
   references rewritten first).
3. **Every `/gnu/store` reference** — input derivations, builder, args, env — is
   rewritten to the already-translated `/nix/store` counterpart.
4. **Output paths are blanked** and the derivation is registered via
   `nix derivation add` (JSON format v4), which lets the Nix daemon compute the
   canonical output paths and `.drv` path itself.

There is deliberately **no** `stdenv` substitution and **no** bootstrap
"boundary": the seeds translate like everything else. See
[`DESIGN.md`](DESIGN.md) for the architecture and [`NOTES.md`](NOTES.md) for the
empirical log (including a few dead ends, like why `nix-store --add` can't
register a `.drv`).

## Getting started

You need `nix` (with the `nix-command` experimental feature) and a working
`guix` to generate the input derivations.

```console
# build it
❯ nix-shell -p cargo rustc gcc --run "cargo build --release"

# generate a Guix derivation
❯ guix build hello --derivations
/gnu/store/...-hello-2.12.2.drv

# translate it (prints the Nix .drv on stdout; logs go to stderr)
❯ ./target/release/guix-transfer /gnu/store/...-hello-2.12.2.drv
/nix/store/...-hello-2.12.2.drv

# build it with Nix
❯ nix-store --realise /nix/store/...-hello-2.12.2.drv
```

Flags: `-v` for per-derivation logging, `--upstream` to fetch from the original
mirrors (ranked + probed) instead of the Guix CA mirror.

## Examples

A ladder of `.drv`-generating Scheme snippets, simplest first, lives in
[`examples/`](examples/). Run the whole suite with
[`examples/validate_all.sh`](examples/validate_all.sh).

| # | Example | Exercises | Realises under Nix |
|---|---------|-----------|:------------------:|
| 1 | `minimal` | raw `/bin/sh` derivation | ✅ → `Success` |
| 2 | `fod` | `builtin:download` → `builtin:fetchurl` | ✅ (downloads + hash-checks) |
| 3 | `dependencies` | a 2-level graph with an output reference | ✅ → `Captured: Shared Secret` |
| 4 | `bootstrap-seed` | `%bootstrap-guile`: executable seed downloads + a generated wrapper | ✅ **runs** `guile 2.0.9` under Nix |
| 5 | `m4-boot0` | the early bootstrap chain (140 derivations) | translates clean; realise = full mesboot compile |
| 6 | `hello` | the full hello DAG (228 derivations) | translates clean; realise rebuilds the world |

Examples 1–6 all translate with **zero** leftover `/gnu/store` references.

## Development

```console
❯ nix-shell -p cargo rustc gcc --run "cargo test"
```

The logic that doesn't need a store — ATerm parsing, hash/base32 conversion,
the CA-mirror URL, JSON v4 emission, URL selection — is covered by unit tests
(checked against `nix hash` where relevant).

## Questions

**Is this affiliated with the Guix or Nix projects?** No. It's a personal
experiment.

**Does it produce bit-identical outputs to Guix?** The fixed-output sources are
identical (same content hash). Built outputs are produced by the Nix daemon
from the same inputs; they should be functionally equivalent, but this isn't a
reproducibility claim.

**Why not just use `guix-daemon`?** That would defeat the point — the goal is to
show a Guix graph building under *Nix*, because the two are closer than they
look.

## License

MIT. Not affiliated with the GNU Guix or NixOS projects.
