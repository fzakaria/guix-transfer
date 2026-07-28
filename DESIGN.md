# DESIGN.md — guix-transfer

How `guix-transfer` translates a GNU Guix derivation graph into a Nix
derivation graph that the Nix daemon can build.

> This is a forward-looking design document. The empirical log — including the
> dead ends that shaped these decisions — lives in [`NOTES.md`](NOTES.md).

---

## 1. Goal & key insight

Build a Guix package under Nix without porting it to a Nix expression.

A `.drv` — in *both* systems — is an [ATerm](https://en.wikipedia.org/wiki/ATerms)
`Derive(...)` record, and `nix-daemon` / `guix-daemon` are both just sandboxed
builders that consume one and produce its outputs. A Guix derivation is already
hermetic: it names every input derivation, every source, the builder, args and
env. So translation, not reimplementation, is enough.

The differences between the two are small and mechanical:

| Aspect          | Guix                          | Nix                              |
|:----------------|:------------------------------|:---------------------------------|
| Store prefix    | `/gnu/store`                  | `/nix/store`                     |
| Output hashing  | same algorithm, store dir `/gnu/store` | same algorithm, store dir `/nix/store` → **different paths** |
| Source fetcher  | `builtin:download` (mirror list) | pinned `pkgs.fetchurl { urls = […] }` |
| FOD hash form   | base16 + algo `sha256`/`r:sha256` | SRI + method `flat`/`nar`     |

Because output paths fold in the store directory, we cannot just textually swap
`/gnu/store` → `/nix/store` in an output path; the path must be **recomputed**.
We let the Nix daemon do that for us (see §3.1).

The whole graph translates this way, *including the bootstrap seeds* — there is
no special-casing of the toolchain (§4).

---

## 2. Pipeline

```
            /gnu/store/…-X.drv  (root Guix derivation)
                     │
              graph.rs · load_recursive()
              • parse every reachable .drv
              • emit a post-order (dependencies first)
                     │
              splicer.rs · for each drv, bottom-up:
              ├─ builtin:download → pinned pkgs.fetchurl (ordered URLs)      §5
              ├─ add input sources to the Nix store (rewrite text)          §3.3
              ├─ rewrite every /gnu/store ref via the guix→nix map          §3.2
              ├─ blank own output paths                                     §3.1
              └─ register with `nix derivation add`; record output paths    §3.1
                     │
            /nix/store/…-X.drv  (Nix derivation)  →  nix-store --realise
```

### Modules

| Module        | Role |
|:--------------|:-----|
| `parser.rs`   | ATerm `Derive(...)` → `ast::Derivation` (nom). |
| `ast.rs`      | AST types, ATerm `Display`, store-path/name helpers. |
| `graph.rs`    | Recursively load the `.drv` DAG; post-order topological sort. |
| `hash.rs`     | Pure hash logic: base16→SRI, base16→nix-base32, CA-mirror URL, flat/nar. |
| `mirrors.rs`  | Guix URL extraction and deterministic `mirror://` expansion. |
| `fetchurl.rs` | Shared pinned-nixpkgs `fetchurl { urls = …; }` renderer and source metadata. |
| `json.rs`     | `Derivation` → Nix JSON derivation, **format version 4**. |
| `nixstore.rs` | Wrappers over `nix derivation add` / `nix derivation show` / `nix-store --add`. |
| `emit_nix.rs` | `--emit-nix`: generate a standalone `.nix` file from translated derivations. |
| `splicer.rs`  | Per-derivation translation, bottom-up; owns the guix→nix path map. |
| `main.rs`     | CLI (`-v`, `--upstream`, `--emit-nix`); prints the final `.drv` to stdout. |

Each module is unit-tested for the store-independent logic.

---

## 3. Translating one derivation

The splicer keeps a single map, **`guix path → nix path`**, covering every
`.drv` path *and* every output path it has produced so far. Because it processes
in dependency order, every reference a derivation makes is already in the map by
the time it is translated.

### 3.1 Registration & path computation

Derivations are registered with **`nix derivation add`** (reads a JSON
derivation, format v4, on stdin). Crucially, we emit the outputs with **empty
paths**; the daemon computes them via `hashDerivationModulo` — the same scheme
Guix uses (§6) — and returns the canonical `text:`-addressed `.drv` path. We
then read the computed output paths back with `nix derivation show` and add them
to the map for parents to reference.

This means **we never compute a Nix hash ourselves.** It also sidesteps the
chicken-and-egg of input-addressed outputs: a derivation's own output paths are
blanked (both in the `outputs` list and in any env var named after an output)
before registration, exactly as Nix does internally.

> `nix-store --add` is *not* used for derivations: it content-addresses the file
> as a `source:` path (a doubled hash, `<new>-<guixhash>-name.drv`) whose baked
> -in output paths no longer match, so `nix-store --realise` fails on it. See
> NOTES.md. `nix-store --add` *is* the right tool for plain input sources (§3.3).

### 3.2 Path rewriting

For each derivation we rewrite, using the map:

- **input derivations** — each `(drv path, outputs)` key is remapped to the Nix
  `.drv` path;
- **builder, args, env values** — every occurrence of a known Guix store path is
  replaced with its Nix counterpart. Store paths are fixed-shape
  (`/…/store/<32-char-hash>-name`) with content-derived hashes, so there are no
  prefix collisions between distinct entries.

A leftover `/gnu/store` string after rewriting means a dependency was missed; the
splicer logs a warning rather than blindly swapping the prefix (which would
fabricate a non-existent path). In practice the count is zero for the whole
`hello` graph.

### 3.3 Input sources

`input_srcs` are plain files/dirs Guix added to its store (build scripts, mirror
lists, …). Text files have their embedded store paths rewritten with the current
map, then are staged under their clean name and added with `nix-store --add`
(producing a `source:` path, valid as an input source). Binaries and
directories are added as-is. Download derivations don't need their Guix mirror
sources, so those are dropped (§5).

---

## 4. The bootstrap: why there is no boundary

The tempting idea — detect Guix's bootstrap toolchain and map it onto Nix's
`stdenv.cc` — is both unnecessary and wrong (it conflates a compiler wrapper
with libc/coreutils and mixes ABIs). We translate *everything*, and the chain
closes on its own:

- **The seeds are downloads.** At the very bottom, Guix's graph is ~80
  `builtin:download` FODs: source tarballs *and* the seed binaries (the i686
  `bash`/`tar`/`mkdir`/`xz`, `static-binaries.tar.xz`, the bootstrap guile, …).
- **The seed binaries are statically linked.** They have no `PT_INTERP` and no
  baked-in `/gnu/store` RPATH, so they execute in the Nix sandbox regardless of
  store prefix.
- **Everything else is regenerated.** The remaining `/gnu/store` strings live in
  build *products* (e.g. the `guile-bootstrap` wrapper script, written by an
  input-addressed build step) or in env/args we already rewrite. When the build
  runs under Nix, those products come out pointing at `/nix/store`.

So once the seeds are fetched and the leaf builds run, `mes → tcc →
gcc-mesboot → glibc → guile → gcc → … → hello` builds organically in Nix from
Guix's own sources. (Verified bottom-up: the translated `%bootstrap-guile`
builds and runs `guile 2.0.9` under Nix; realising `hello` proceeds to compile
`mes` from source.)

---

## 5. Source fetching: deterministic build-time fallback

Guix's `builtin:download` is translated to the pinned nixpkgs
`pkgs.fetchurl { urls = [ … ]; }` helper.  The fixed-output derivation records
its complete ordered candidate list; nixpkgs' fetcher tries candidates at
**build time**. Translation never contacts a candidate URL. Therefore an
outage cannot change the translated `.drv` or output path.

### 5.1 Candidate policy

The candidate list is constructed purely from the Guix derivation:

1. In default mode, prepend Guix's content-addressed (CA) mirror when the
   download has a supported SHA-256 fixed-output hash. Its deterministic URL is
   `https://bordeaux.guix.gnu.org/file/<name>/sha256/<nix-base32(hash)>`, where
   `<name>` is the output store name. The CA mirror is used for flat,
   recursive, and executable downloads: pinned nixpkgs `fetchurl` was
   empirically verified with `urls`, `recursiveHash`, and `executable` for
   these modes.
2. Extract the original Guix URL declaration in declaration order. For a
   `mirror://` declaration, read the serialized `%mirrors` input source carried
   by that *same Guix download derivation*, parse its complete table, and append
   **every** base URL from the matching entry in table order. Scheme matching is
   longest-prefix (`gnu/alpha` wins over `gnu` when both exist); if only `gnu`
   exists, `mirror://gnu/alpha/...` correctly uses `alpha/...` as its path.
   Thus the table is not duplicated or stale in this repository. The generated
   candidates are concrete strings, so neither direct translation nor either
   emitted form retains a Guix-store dependency. Omit an unknown `mirror://`
   scheme because it is not a fetchable URL without Guix's runtime machinery.
   Keep every other concrete URL in order.
3. De-duplicate the complete list stably, retaining the first occurrence.

`--upstream` omits only the CA candidate. It still performs complete,
deterministic mirror expansion and stable de-duplication, preserving Guix
declaration and mirror-table order. There is no host ranking, URL probe,
availability-dependent selection, or translation-time network request in either
mode.

### 5.2 Shared fetcher and identity

Translation instantiates the same pinned `pkgs.fetchurl` helper that generated
Nix expressions import. It obtains `drvPath` and `outPath` through `nix eval`
without building the FOD, maps the Guix download derivation and output to those
paths, and records the helper arguments for emitters. The helper receives the
Guix download derivation's concrete `system` and selects
`legacyPackages.${system}.fetchurl` in all three forms. The only normalization
is Guix's literal `builtin` system: it has no nixpkgs key, so the helper uses
`builtins.currentSystem`; inspected real Guix downloads carry concrete systems
such as `x86_64-linux`. `--emit-nix` and `--emit-nix-dir` render the same helper
call and ordered list. Thus direct registration, the single emitted expression,
and the directory emitter have identical source `.drv` and output identities;
consumers are rewritten against those identical paths.

**Hash translation.** Guix's base16 SHA-256 is converted to SRI. `r:sha256`
sets `recursiveHash = true`; the Guix `executable=1` flag sets both
`recursiveHash = true` and `executable = true`; other downloads are flat.

### 5.3 Failure semantics

The helper falls through transport and HTTP failures in list order. If every
candidate fails, nixpkgs reports that no mirror succeeded and its log identifies
all attempted URLs. A successfully transferred object with a wrong fixed-output
hash is a hash mismatch, not a fallback success: Nix fails the build
authoritatively without trying a later candidate. The focused integration test
covers a refused loopback connection, HTTP failure, and a wrong-first/
correct-second pair whose request log proves the second URL was not fetched.
Fixtures are loopback HTTP because `file://` paths are unavailable inside the
Nix sandbox; no public source URL is needed.

---

## 6. Reference: ATerm & path computation

Both systems store derivations on disk as the same ATerm:

```
Derive(
  [(output-name, output-path, hash-algo, hash), ...],
  [(input-drv-path, [output-names]), ...],
  [input-src-paths, ...],
  system, builder, [args, ...],
  [(env-key, env-value), ...]
)
```

`guix-transfer` does not compute the paths below — `nix derivation add` does —
but they explain why a textual prefix swap is insufficient and why the CA-mirror
URL (which depends only on content hash) is store-prefix-independent.

**Input-addressed output path** for output `name`:

```
hash      = sha256(aterm_modulo)            # outputs blanked; input drvs
                                            # replaced by their own modulo hash
path      = store_dir / base32(compress(sha256(
              "output:" + name + ":sha256:" + hex(hash) + ":" + store_dir + ":" + name
            ), 20)) + "-" + name
```

**Fixed-output path:**

```
path = store_dir / base32(compress(sha256(
         "fixed:out:" + algo + ":" + hash + ":" + store_dir + ":" + name
       ), 20)) + "-" + name
```

The `.drv` file itself is a `text:` object whose hash covers the final ATerm and
whose references are its input drv + src paths. The store directory appears in
every one of these, which is why Guix and Nix paths differ and must be
recomputed rather than rewritten.

---

## 7. `--emit-nix`: generating standalone Nix expressions

`--emit-nix <output.nix>` produces a single `.nix` file that reconstructs
the entire translated derivation graph using `builtins.derivation` calls
inside a `let … in` block. The root derivation is the final expression.

### 7.1 Dependency tracking via string context

`builtins.derivation` tracks dependencies through **string context**, not
explicit input lists like `nix derivation add`'s JSON format. For the emitted
`.nix` to produce identical derivation hashes:

- **Derivation dependencies** are `let` bindings, referenced via `${dep}` string
  interpolation → tracked as `inputDrvs`.
- **Input sources** use `builtins.storePath /nix/store/…` → tracked as
  `inputSrcs`.
- **FODs** use `outputHash`/`outputHashAlgo`/`outputHashMode` attributes.

### 7.2 `builtins.derivation` env var injection

Nix's `builtins.derivation` (via `derivationStrict` in `primops.cc` line 1692)
unconditionally copies `name`, `system`, and `builder` into `drv.env`. Guix
derivations do not include these in their env vars.

To ensure `nix derivation add` and `builtins.derivation` produce identical
hashes, the splicer injects `name`, `system`, `builder` into env during
translation if not already present.

### 7.3 Phantom dependencies

Some derivations reference input drv outputs only inside inputSrc files (e.g. a
build script that calls `mkdir`, `tar`, `xz` by store path). These paths don't
appear in any derivation attribute, so `builtins.derivation` can't detect them
via string context.

The splicer detects such "phantom" dependencies — input drv outputs not
referenced in builder/args/env — and surfaces them in a `__phantom_deps` env
var. This env var is emitted in both `nix derivation add` and the `.nix`
expression, so Nix tracks the dependencies and makes them available in the
build sandbox.
