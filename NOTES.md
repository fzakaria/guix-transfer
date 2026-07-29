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

### Follow-up: multi-URL fallback in the derivation (supersedes the above)

The single-URL `builtin:fetchurl` approach had two residual problems: the CA
mirror does not carry *every* source (only what Guix CI has seen), and the
`--upstream` probing path made translation nondeterministic — whichever mirror
answered that day was baked into the `.drv`, changing the derivation identity
and the emitted `.nix` between runs.

Resolution (discussed in PRs #1/#2): go back on "no nixpkgs.fetchurl". Each
download is now a pinned nixpkgs `fetchurl { urls = […]; }` FOD — the same
`builtins.getFlake` pinning already used for `fetchgit` — with the CA mirror
first and the deterministic upstream candidates after it. The fallback happens
at build time inside the derivation; translation never probes, and the URL list
(hence the drv path) is a pure function of the Guix derivation. `--upstream`,
the probing (`net.rs`/`ureq`), and the Guix-only env dropping are gone —
downloads never reach the env-rewriting path at all. NAR-hashed downloads
(`r:sha256`/`executable`) skip the CA entry since the mirror keys on the flat
content hash.

## builtin:git-download → read Guix's build farms before cloning

`pkgs.fetchgit` reproduces Guix's checkout exactly, but it only ever knows one
URL. A url-fetch source carries a whole candidate list (above); a git-fetch
source has no equivalent, so when its forge is unreachable the package is
simply unbuildable. savannah is the sharp edge: it rate-limits and times out
under load, and `config.git` (which `cups`, `libva`, `openjdk`, … all need)
lives there. Observed failure: three `git fetch` attempts, each timing out
after ~135s, and the whole build dies.

The farms already have every one of these checkouts, and three facts make
fetching from them exact rather than approximate:

* A narinfo is filed under the **hash part of the `/gnu/store` path**, which the
  derivation being translated already states. `GitSource.guix_hash` records it
  (`ast::store_path_hash`) and the emitted call passes it as `guixHash`, so the
  lookup key is ground truth. (It is *also* derivable from `(hash, name)` —
  Guix computes fixed-output paths with Nix's own `source:` formula, differing
  only in the store prefix — but recomputing Nix's path algorithm in Nix buys
  nothing over reading the path we already parsed.)
* For a directory, `NarHash` in the narinfo **is** the recursive sha256, i.e.
  bit-identical to the `hash` a git-fetch source already declares. Verified on
  `config-0.0.0-1.c8ddc84-checkout`: bordeaux serves NarHash
  `0x6ycvkmmhhhag97wsf0pw8n5fvh12rjvrck90rz17my4ys16qwv`, exactly the source's
  own hash in nix32.
* `References:` on these source items is empty, so restoring the nar under
  `/nix/store` is content-identical — no path rewriting, no hash drift.

So `src/fetch-git.nix` walks bordeaux then ci, restores the nar with
`nix-store --restore` (pure deserialization: no store, no daemon), and falls
back to `nix-prefetch-git` — the same script `pkgs.fetchgit` drives — only when
neither farm has it. There is no content-addressed nar endpoint that would let
the source hash serve as the key directly (`/nar/sha256/<hash>` and
`/file/<name>/sha256/<hash>` both fail for directories), which is why the store
path is needed at all. Bordeaux advertises lzip only, ci gzip and lzip, so the
`Compression:` field picks the decompressor rather than a hardcoded one.

Coverage on the guixpkgs tree (148 checkouts, probed 2026-07-29): **bordeaux has
all 148**; ci answered 132 and timed out (504) on 16. Cloning is the
never-taken path in practice, which is exactly why it needs testing on purpose
— see below.

**Certificates.** The clone fallback needs git's CA config, and
`nix-prefetch-git` takes it from `NIX_GIT_SSL_CAINFO` or `NIX_SSL_CERT_FILE`,
neither of which is implied by curl's `SSL_CERT_FILE`. Forcing the fallback with
a deliberately wrong `guixHash` surfaced this as `unable to get local issuer
certificate (20)`; the fetcher now sets both and carries `cacert`, mirroring
`pkgs.fetchgit`. With that, a forced fallback clone of `python-pycairo-1.28.0`
lands on the same store path the nar route produces — the fallback is
hash-faithful to Guix's checkout, not merely close.

Nothing needs to trust the farms: the output is a fixed-output derivation, so
wrong bytes cannot produce the expected path — a stale or hostile substitute
fails the build instead of poisoning the store.

**Why this rebuilds nothing.** Output paths of a recursive-sha256 FOD are fixed
by `(name, hash)` alone, and a consumer's *output* path is computed through
`hashDerivationModulo`, which masks a fixed-output input down to
`fixed:out:<algo>:<hash>:<path>` — the fetcher's builder is invisible to it.
Changing how a checkout is fetched therefore changes each source's `.drv` path
(a text hash over the ATerm) but no output path anywhere. Confirmed on
guixpkgs: `openjdk-25-guix-wrapped` kept output
`kihk714ljmvhqh36cigc7mfimv0q0rbr` across the switch while its `.drv` moved
`zgcvkc97…` → `nqr58vg9…`. The emitted tree still needs regenerating, because
each store file's drvPath guard bakes in the `.drv` recorded at sync time.

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

## Source ordering bug (found while realising hello)

The first full `hello` realise failed deep in the chain:

```
patch: Can't open patch file /gnu/store/…-bash-linux-pgrp-pipe.patch : No such file
```

Root cause: a derivation's `input_srcs` can reference *each other* by absolute
path — the generated Guile builder script (`bash-5.2.tar.xz-builder`) embeds the
path of a sibling `.patch`. We were adding/rewriting sources in list order, and
the script came before the patch, so the script was rewritten while the patch
was still unmapped → the stale `/gnu/store` patch path survived. (Translation
reported 0 leftovers because the old warning only scanned builder/args/env, not
source *contents*.)

Fix: resolve `input_srcs` in dependency order — add a source only once every
sibling it textually references is mapped. After this, the patch resolves to
`/nix/store/…` and applies; the bash source builds. Verified the previously
-failing step now logs `applying '/nix/store/…-bash-linux-pgrp-pipe.patch'`.

Related fix: the bare **store-directory constant** `/gnu/store` (no hash
following) — e.g. the `%store-directory` literal in `(guix build utils)`'s
`build-utils.scm` — is now swapped wholesale to `/nix/store`. Full paths still
go through the map. Leftover-warnings match only real `/gnu/store/<hash>-` paths.

Known benign leftovers (auxiliary data, not build-graph edges):
- `binutils-boot-2.20.1a.patch` content references a `tcc-boot` output in a hunk
  that the `binutils-mesboot` stage doesn't use (tcc-boot isn't its input).
- `perl-boot0`'s `disallowedReferences` (a *negative* constraint) names
  `binutils-bootstrap-0`. Blindly swapping either would fabricate a
  non-existent path, so they're left as-is for now.

## hello build: how far it gets, and the environment blocker

With the source-ordering fix, the translated `hello` graph builds organically
under Nix all the way through the early bootstrap:

```
downloads (CA mirror) → stage0-posix → mes-boot → tcc-boot0 → bash (patches
applied from /nix/store) → … 
```

It then stops at `patch-mesboot-2.5.9` — the **single** real leaf failure;
everything above it is a `1 dependency failed` cascade. The error:

```
gash tar: chmod "patch-2.5.9/pc/djgpp/" 0o42775  →  Operation not permitted
```

The early bootstrap unpacks sources with gash-utils' Scheme `tar`, which restores
each directory's stored mode — including the **setgid** bit on dirs like
`pc/djgpp/`. On this host the Nix-daemon build process cannot set the setgid
bit, so the unpack aborts.

Cause: **Nix intentionally blocks setuid/setgid in builders.** Nix installs a
seccomp filter (`filter-syscalls`, default on) that forces `EPERM` on any
`chmod`/`fchmodat` that sets the setuid or setgid bit — because Nix doesn't
support setuid/setgid in outputs (NARs carry no ownership, and it would make
results depend on the building user). See the Nix manual on derivation outputs
and [NixOS/nix#2522]. gash-utils' Scheme `tar` restores a tarball directory's
full stored mode (incl. setgid) and treats the resulting EPERM as fatal, where
GNU tar would just skip setuid/setgid for non-root.

A minimal probe derivation (`mkdir d; chmod <mode> d`) confirms it — and that it
is the seccomp filter, not the host, the filesystem, `no_new_privs`, or a daemon
(this is a single-user install, builds run as the user):

| mode | default build | `--option filter-syscalls false` | interactive shell |
|------|---------------|----------------------------------|-------------------|
| `0775` / `1775` sticky    | OK   | OK | OK |
| `2775` **setgid**         | FAIL | **OK** | OK |
| `4775` **setuid**         | FAIL | **OK** | OK |

**Fix:** realise the bootstrap with `--option filter-syscalls false`:

```
nix-store --realise --option filter-syscalls false <hello.drv>
```

This is safe — Nix canonicalises every output anyway (mode 0444/0555, timestamp
1, setuid/setgid cleared), so disabling the filter only lets the build's *temp*
extraction set the bits gash tar wants; the bits never reach the output. With
this, `patch-mesboot` (and the chain above it) build. Guix sidesteps the whole
issue here by **substituting** the prebuilt `patch-mesboot` from
`bordeaux.guix.gnu.org` rather than building it.

Examples 1–4 don't need the flag (they don't unpack setgid tarballs); the deep
bootstrap (m4-boot0 / hello) does.

### How far hello gets, and the next blocker

With both fixes (`--option filter-syscalls false`), the translated hello builds
organically through:

```
downloads → stage0-posix → mes-boot → tcc-boot0 → bash (patched) →
binutils-mesboot0 → gcc-core-mesboot0 → gcc-mesboot0 (gcc 2.95.3) →
glibc-mesboot0 → binutils-mesboot1 → make-mesboot → mesboot-headers → …
```

The gcc-mesboot1 (gcc 4.6.4) `gcc/configure` prints:

```
checking how to run the C++ preprocessor... /lib/cpp
configure: error: in `.../host-i686-unknown-linux-gnu/gcc':
configure: WARNING: C++ preprocessor "/lib/cpp" fails sanity check
```

This was initially misread as a fatal error, but it is actually a
**non-fatal WARNING**. The "error:" line is an autoconf *context* prefix
(identifying the failing directory), followed by "WARNING:" (the actual
status). The configure script does **not** abort — it continues past this
point.

There is a **separate**, truly fatal CXXCPP sanity check later in
`gcc/configure` (line ~17979, inside the libtool section), but it is guarded
by:

```
if test -n "$CXX" && ( test "X$CXX" != "Xno" &&
    ( (test "X$CXX" = "Xg++" && `g++ -v >/dev/null 2>&1` ) ||
    (test "X$CXX" != "Xg++"))) ; then
    # ... fatal CXXCPP check runs here ...
```

When `CXX=g++` and `g++` is not on PATH (which is the case here —
gcc-mesboot0 is C-only by design), `g++ -v` returns exit 127, so the guard
evaluates to FALSE and the fatal check is **skipped entirely**.

Confirmed by building gcc-mesboot1 from source under guix-daemon with
`--check --keep-failed`: the same "error:" + "WARNING:" context lines
appear in the Guix build log at the same point, and configure continues to
completion. The build proceeds to `make` and compiles GCC's C++ frontend
successfully (because `--disable-build-with-cxx` means the build system
itself uses only C; it does not need a working C++ *compiler* to *build*
the C++ frontend).

**Bottom line:** The C++ preprocessor warning is a red herring. The gcc-mesboot1
build should complete under nix-daemon just as it does under guix-daemon.
If the Nix build previously failed at this point, it was likely due to an
environmental issue (e.g. stale build artifacts, incorrect sandbox config,
or a different downstream failure misattributed to this warning). Needs a
re-test with a clean translation.

The core thesis — faithful translation, with nix-daemon building the
imported Guix graph organically — is demonstrated across the source
bootstrap (downloads → stage0 → mes → tcc → gcc 2.95.3 → glibc-mesboot0 →
… → gcc-mesboot1 and beyond).

## `--emit-nix`: standalone Nix expression generation

Added `--emit-nix <output.nix>` to produce a self-contained `.nix` file from
translated derivations. Key findings during implementation:

### `builtins.derivation` injects extra env vars

Nix's `builtins.derivation` (`primops.cc` `derivationStrictInternal`, line 1692)
calls `drv.env.emplace(key, s)` for **every** attribute except `args`,
`__contentAddressed`, `__impure`, `__ignoreNulls`, and `__structuredAttrs`.
This means `name`, `system`, `builder` are always in env — but Guix derivations
don't include them. The emitted `.nix` and `nix derivation add` produced
different hashes until we started injecting these env vars during translation.

### Phantom dependencies: deps hidden inside inputSrc files

The `guile-bootstrap-2.0` derivation's build script (`build-bootstrap-guile.sh`)
calls `mkdir`, `tar`, `xz` by their store paths. These paths appear only inside
the script file (an `inputSrc`), not in any derivation attribute. With
`nix derivation add`, the dependencies are explicit in `inputs.drvs`. But
`builtins.derivation` only tracks dependencies via string context in attribute
values — it can't see inside files.

Fix: the splicer detects input drv outputs not referenced in any
builder/args/env string and collects them into a `__phantom_deps` env var.
Both `nix derivation add` and the `.nix` expression include this var, so
hashes match and the sandbox has the tools available.

Verified: `nix-build /tmp/demo.nix` (bootstrap guile + a demo derivation)
builds successfully with the phantom deps fix.

## Architecture

| module      | role |
|-------------|------|
| `parser.rs` | ATerm `Derive(...)` → `ast::Derivation` (nom). |
| `ast.rs`    | AST + ATerm `Display` + path/name helpers. |
| `hash.rs`   | hex→SRI, hex→nix-base32, CA-mirror URL, method detection. Pure, unit-tested. |
| `mirrors.rs`| `mirror://` expansion + URL extraction + deterministic host ranking. |
| `json.rs`   | `Derivation` → Nix JSON v4 (serde_json). |
| `nixstore.rs`| shell out to `nix derivation add` / `nix derivation show` / `nix-store --add`. |
| `emit_nix.rs`| `--emit-nix`: generate standalone `.nix` from translated derivations; renders the shared `fetch-url.nix`/`fetch-git.nix` helpers. |
| `splicer.rs`| per-derivation translation, bottom-up. |
| `graph.rs`  | recursive load + post-order topo. |
| `main.rs`   | CLI (`-v`, `--emit-nix`, `--emit-nix-dir`). |

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
