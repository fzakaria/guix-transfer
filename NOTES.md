# NOTES.md — Working log & findings

Running log of empirical findings while building the Guix→Nix splicer.
Newest insights at the top of each section.

## Environment

- `nix` 2.34.7, `guix` 1.5.0 both present.
- No `cargo`/`rustc` on PATH — use `nix-shell -p cargo rustc gcc --run "..."`.
- `<nixpkgs>` resolves (flake registry) even though `NIX_PATH` is empty.
- Nix sandbox provides `/bin/sh` via `sandbox-paths =
  /bin/sh=/nix/store/...-busybox/bin/busybox`. So Guix derivations whose
  builder is literally `/bin/sh` (examples 1 & 3) build unmodified.

## Registration: how to put a translated `.drv` into the Nix store  (resolves DESIGN §4.1)

**Do NOT use `nix-store --add`** — it content-addresses the file as a `source:`
path, which does not match the canonical `text:` derivation path, so parents
referencing the child by path would break.

**Use `nix derivation add`** (experimental `nix-command`). It reads a JSON
derivation (format **version 4**) on stdin, computes the output paths and the
canonical `.drv` path itself, registers it with the daemon, and prints the
path. Verified end-to-end:

- Input JSON may leave output `path` empty (`"outputs":{"out":{}}`) — Nix fills
  it in via `hashDerivationModulo`. We never compute Nix hashes ourselves.
- Must include `"version":4`. Version 3 is rejected by 2.34.
- After adding, read back computed output paths with `nix derivation show
  <drv>` to build the guix→nix output-path map for parents.

This eliminates the entire "hash-convergence loop" (old splicer.rs:148-192).

### JSON v4 shape (plain, non-structuredAttrs derivation)

```json
{ "version":4, "name":"minimal", "system":"x86_64-linux",
  "builder":"/bin/sh", "args":["-c","echo 'Success' > $out"],
  "env":{"PATH":"/bin","out":""},
  "inputs":{"drvs":{},"srcs":[]},
  "outputs":{"out":{}} }
```

- `inputs.drvs` keys are full `/nix/store/...drv` paths → `{"outputs":["out"],"dynamicOutputs":{}}`.
- `inputs.srcs` is a list of full `/nix/store/...` paths.
- Fixed-output: `"outputs":{"out":{"hash":"sha256-<base64>","method":"flat"|"nar"}}`.
  Hash is **SRI** (`sha256-` + base64 of raw digest). Guix gives lowercase hex,
  so we convert hex→base64. `method:"nar"` for recursive (`r:sha256`) / executable
  downloads, `"flat"` otherwise.

## builtin:download → builtin:fetchurl  (resolves DESIGN §4.3; user: keep it builtin, no nixpkgs.fetchurl)

Nix's own `builtin:fetchurl` is enough — no need for nixpkgs `fetchurl`.
A minimal translated download derivation:

```json
{ "version":4, "name":"hello-source", "system":"builtin",
  "builder":"builtin:fetchurl", "args":[],
  "env":{"out":"","url":"https://.../hello-2.12.tar.gz"},
  "inputs":{"drvs":{},"srcs":[]},
  "outputs":{"out":{"hash":"sha256-...","method":"flat"}} }
```

Verified: realises and downloads, hash-checks. (The hand-written hash in
`examples/2-fod.scm` was wrong; the real hello-2.12.tar.gz is
`sha256-zwSvhtwIUmjF9EcPuuSbGK+8Iht4CWqrhC2TSna60Ks=`.)

Details:
- Drop Guix-specific download inputs/env: `mirrors`, `disarchive-mirrors`,
  `content-addressed-mirrors`, `impureEnvVars`, `preferLocalBuild`.
- `executable` env `"1"` (e.g. the bootstrap `bash` download) → method `nar` +
  keep `executable` env so fetchurl chmod +x and hashes recursively.

### URL selection — use the Guix content-addressed mirror (the key fix)

`builtin:fetchurl` takes exactly ONE url and **cannot fall back** across a
list, but the upstream lists are unreliable: `lilypond.org/janneke` 404s; the
guile bootstrap tarball is only on `alpha.gnu.org` (not ftp/ftpmirror); the
i686 seed binaries (`tar`/`bash`/`mkdir`/`xz`) are git-only and `cgit`
rate-limits with flaky 301s. Static host-scoring and even live probing are
fragile.

**Solution (the user's idea):** rewrite every download to Guix's
content-addressed mirror, which serves *any* source Guix CI has seen, keyed by
content hash — exactly what we already have. Format (from the
`content-addressed-mirrors` file):

```
https://bordeaux.guix.gnu.org/file/<name>/sha256/<nix-base32(hash)>
```

`<name>` = the output's store name (e.g. `hello-2.12.2.tar.gz`, `tar`). The
hash bytes are the FOD sha256 (strip any `r:` prefix). Verified: the previously
-404ing mes tarball, the alpha-only guile tarball, and the cgit-only `tar`
binary all return 200 and hash-match when realised through `builtin:fetchurl`
(recursive/executable `tar` and flat tarballs alike). This is the default
(`hash::guix_ca_mirror_url`). `--upstream` switches to the original mirror list
with reliability ranking + probing (`mirrors.rs` + `net.rs`) as a fallback.

We also confirmed an alternative that works identically: `guix build <drv>`
then `nix-store --add-fixed [--recursive] sha256 <staged>` reproduces the exact
fetchurl output path — i.e. transplanting Guix's local output. The CA-mirror
URL is cleaner (pure fetchurl, no `guix build`), so that's what we ship.

## The bootstrap chain is fully translatable — NO stdenv mapping needed  (revises DESIGN §4.2)

Inspected `m4-boot0` (example 4): 140 `.drv` in closure. Builders are only:
`builtin:download` (the ~84 seed/tarball FODs) and the bootstrap
`guile`/`bash`. Key findings:

- The seed **binaries** (`bash`, `mkdir`, `tar`, `xz`, `static-binaries.tar.xz`,
  guile tarball, …) are all `builtin:download` FODs. The bash seed is
  `ELF 32-bit, statically linked` with **no PT_INTERP** → runs in any sandbox
  regardless of store prefix.
- `guile-bootstrap-2.0` is **input-addressed** (not an FOD): a build script
  (`build-bootstrap-guile.sh`) unpacks the guile tarball and writes a wrapper.
  `.guile-real` is statically linked; the only `/gnu/store` strings are in the
  generated bash **wrapper** (shebang + `GUILE_SYSTEM_PATH` exports + exec
  path) — all produced at build time, so they come out as `/nix/store` once we
  rewrite the builder's inputs/env.

**Conclusion:** every `/gnu/store` reference is either (a) inside a build
product we regenerate, or (b) in env/args/builder we rewrite. The downloaded
seeds are content-locked but position-independent (static). Therefore the whole
graph can be translated derivation-by-derivation and built organically by the
Nix daemon. DESIGN's "boundary regex → stdenv.cc" is unnecessary and was the
wrong model (ABI/role mismatch). We translate *everything*.

Open risk to validate during integration: 32-bit static seed execution needs
host ia32 support; and the deep mesboot chain is long (build time), not
conceptually blocked.

## Architecture

| module      | role |
|-------------|------|
| `parser.rs` | ATerm `Derive(...)` → `ast::Derivation` (nom). |
| `ast.rs`    | AST + ATerm `Display` + path/name helpers. |
| `hash.rs`   | hex→SRI, hex→nix-base32, CA-mirror URL, method detection. Pure, unit-tested. |
| `mirrors.rs`| `mirror://` expansion + URL extraction + host ranking (upstream mode). |
| `net.rs`    | curl URL reachability probe (upstream mode). |
| `json.rs`   | `Derivation` → Nix JSON v4 (serde_json). |
| `nixstore.rs`| shell out to `nix derivation add` / `nix derivation show` / `nix-store --add`. |
| `splicer.rs`| per-derivation translation, bottom-up. |
| `graph.rs`  | recursive load + post-order topo. |
| `main.rs`   | CLI (`-v`, `--upstream`). |

## Results (verified end-to-end on this machine)

| Example | What | Status |
|---------|------|--------|
| 1 minimal | raw `/bin/sh` derivation | ✅ realises → `Success` |
| 2 fod | `builtin:download` → `builtin:fetchurl` | ✅ realises, 1 MB tarball, hash-matches (fixed the example's wrong hash) |
| 3 dependencies | 2-level graph, output ref in args | ✅ realises → `Captured: Shared Secret` |
| 4 bootstrap-seed | `%bootstrap-guile`: executable downloads + generated wrapper | ✅ builds **and runs** under Nix (`guile 2.0.9`); wrapper rewritten to `/nix/store` |
| 5 m4-boot0 | early bootstrap chain (140 drvs) | ✅ translates clean (0 leftover `/gnu/store`); realise = full mesboot compile |
| 6 hello | full hello DAG (228 drvs) | ✅ translates clean in ~15 s; realise rebuilds world from source (hours) |

Registration uses `nix derivation add` exclusively (never `nix-store --add` for
`.drv`s — confirmed independently that that produces a doubled-hash `source:`
path whose baked-in output paths don't match, so `--realise` fails).
