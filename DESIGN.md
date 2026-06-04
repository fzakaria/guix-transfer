# DESIGN.md — Guix-to-Nix Splicer

## 1. Goal

Translate a GNU Guix derivation graph into a Nix derivation graph, bottom-up,
so that `nix-store --realise` can build the package. The target milestone is
building `hello-2.12.2` end-to-end.

---

## 2. Architecture Overview

```
                   ┌──────────────────────┐
                   │  /gnu/store/…-X.drv  │  (root Guix derivation)
                   └──────────┬───────────┘
                              │
                 ┌────────────▼────────────┐
                 │   graph.rs              │
                 │   load_recursive()      │
                 │   • parse every .drv    │
                 │   • post-order topo     │
                 └────────────┬────────────┘
                              │
          ┌───────────────────▼───────────────────┐
          │            splicer.rs                  │
          │  for each drv in bottom-up order:      │
          │    1. bootstrap boundary → stdenv.cc   │
          │    2. builtin:download → fetchurl      │
          │    3. FOD shortcut (copy output)        │
          │    4. rewrite input_srcs (scripts)      │
          │    5. rewrite all /gnu/store paths      │
          │    6. hash-convergence loop             │
          └───────────────────┬───────────────────┘
                              │
                   ┌──────────▼───────────┐
                   │  /nix/store/…-X.drv  │  (Nix derivation)
                   └──────────────────────┘
```

### Module responsibilities

| Module       | Lines | Role |
|:-------------|------:|:-----|
| `parser.rs`  |   127 | Parse the ATerm `Derive(...)` format into `ast::Derivation`. |
| `ast.rs`     |   205 | AST types + `Display` (serialise back to ATerm) + `rewrite_paths`. |
| `graph.rs`   |    45 | Recursively load the full `.drv` DAG; produce a topological order. |
| `splicer.rs` |   250 | Core translation logic: boundary detection, fetchurl bridging, path rewriting, hash convergence. |
| `main.rs`    |    53 | CLI entry point; calls `nix-instantiate` for stdenv. |

---

## 3. The Hello Derivation Graph

A `guix build hello --derivations` produces a **deep** dependency DAG.  The
full graph from `hello-2.12.2.drv` down to seeds looks roughly like:

```
hello-2.12.2.drv
  └─ … intermediate drvs (make, gcc, glibc, binutils, ld-wrapper, bash, …)
       └─ gcc-mesboot-3.drv      (stage-3 gcc, built from prior stages)
            └─ gcc-mesboot-2.drv
                 └─ gcc-mesboot-1.drv
                      └─ gcc-mesboot0.drv
                           └─ mes-boot.drv
                                └─ bootstrap-mescc-tools.drv  ← bootstrap binary
                                └─ bootstrap-mes.drv          ← bootstrap binary
       └─ glibc-mesboot.drv
            └─ glibc-headers-mesboot.drv
       └─ bootstrap-binaries.drv   ← the binary seed tarball
       … (100+ derivations total)
```

Key observation: **Guix's bootstrap chain has ~8 stages** of progressively more
capable toolchains before reaching a "modern" gcc/glibc.  The current splicer
attempts to map this to a single Nix `stdenv.cc`.

---

## 4. Identified Issues

### 4.1 ~~CRITICAL~~ NEEDS INVESTIGATION — Derivation registration via `nix-store --add`

**Problem (original theory):** The splicer uses `nix-store --add` to place
`.drv` files into the store. In theory, `nix-store --add` computes a *source*
content-addressed path, not a derivation path. Nix derivation paths are
computed differently (using `hashDerivationModulo`).

**However:** In practice, `nix-store --add` *does* appear to produce a `.drv`
that `nix-store --realise` will attempt to build (confirmed by testing on
NixOS with Guix installed). Nix may be treating any `.drv`-suffixed file in the
store as a derivation and parsing it on `--realise`. This needs further
investigation:

- Does Nix re-hash the `.drv` on `--realise` and move it to the "correct" path?
- Or does it build from wherever the `.drv` landed?
- Does the hash convergence loop (lines 149-192) actually work? The error
  messages it parses ("should be", "incorrect output") may come from
  `nix-store --add` validation after all, or from a subsequent step.

**Action:** Test whether the hash convergence loop fires and whether the
resulting `.drv` path matches what Nix expects. If `nix-store --add` works in
practice for `.drv` files, this is not a blocker — just a correctness concern
for path determinism.

**If it turns out to be a real problem:** Use `nix derivation add` (reads JSON
on stdin, Nix 2.4+) which handles path calculation and registration properly.

### 4.2 CRITICAL — Bootstrap boundary mapping is too simplistic

**Problem:** The regex `r"gcc-bootstrap|glibc-bootstrap|bootstrap-binaries"`
maps matched derivations to `stdenv.cc`.  This has several sub-problems:

1. **One-to-one mapping of many-to-one:** Guix has three distinct bootstrap
   roles (compiler, libc, core-utils). Nix's `stdenv.cc` is a *compiler
   wrapper* — it is not libc, not coreutils.  Replacing `glibc-bootstrap` with
   `stdenv.cc` means downstream builds looking for `libc.so` or `crt1.o` in
   the mapped path will not find them.

2. **Missing bootstrap stages:** The real hello graph goes through
   `bootstrap-mes`, `bootstrap-mescc-tools`, `mes-boot`, `gcc-mesboot0`,
   `gcc-mesboot-1`, `gcc-mesboot-2`, `gcc-mesboot-3`, `glibc-mesboot`,
   `glibc-headers-mesboot`, etc.  None of these match the boundary regex, so
   the splicer will try to build them from source — which requires the Guix
   seed binaries and Guix build environment, defeating the purpose.

3. **ABI incompatibility:** Even if mapping worked, binaries and libraries
   built with the Guix toolchain and Nix toolchain are not ABI-compatible in
   general (different glibc versions, different gcc, different ld flags).

**Fix:** Widen the boundary regex to catch all actual bootstrap seed
derivations. The seeds are the derivations at the very bottom of the DAG that
have no `input_drvs` (or whose only inputs are other seeds). In practice these
are names like `bootstrap-binaries-0`, `bootstrap-mescc-tools`,
`bootstrap-mes`, and similar. The simplest detection: any derivation whose
inputs are all already in `guix_to_nix_map` or that has no inputs and is a
content-addressed/FOD can be treated as a seed.

Alternatively, fetch the Guix bootstrap binaries from the Guix substitute
server and inject them as fixed-output derivations in the Nix store, so
everything above them builds organically.

### 4.3 HIGH — `builtin:download` → `builtin:fetchurl` issues

#### 4.3.1 Single-URL limitation

Nix `builtin:fetchurl` takes exactly one URL. Guix's `builtin:download`
supports a **list** of mirror fallbacks. The splicer takes only the first URL
(line 67). If that URL returns 503 or 404, the build fails with no fallback.

**Fix:** Try URLs in order.  If the first fails, fall back.  Or better: use
Nixpkgs' `fetchurl` (a regular derivation, not the builtin) which supports
`urls = [...]` and mirror resolution.

#### 4.3.2 Incomplete mirror expansion

Only `mirror://gnu/` is expanded.  Guix defines ~30 mirror schemes:
- `mirror://savannah/` → multiple Savannah mirrors
- `mirror://sourceforge/` → SourceForge mirrors
- `mirror://kernel.org/` → kernel.org mirrors
- `mirror://apache/` → Apache mirrors
- `mirror://pypi/` → PyPI mirrors
- etc.

Missing any of these means a URL like `mirror://savannah/hello/hello-2.12.tar.gz`
is passed verbatim to Nix, which doesn't understand it.

**Fix:** Port the Guix mirror list (from `guix/download.scm`) or at minimum
add the most common ones.

#### 4.3.3 Name extraction is broken

Line 43:
```rust
let name = drv_path.split('-').nth(1).unwrap_or("source").replace(".drv", "");
```

Guix store paths look like `/gnu/store/abcdef...xyz-hello-source.drv` where the
32-char base32 hash has **no dashes**. Splitting by `-` yields:
```
["/gnu/store/abcdefxyz", "hello", "source.drv"]
```

So `nth(1)` gives `"hello"`, discarding `"source"`. For a derivation named
`hello-2.12.tar.gz`, you'd get `"hello"` instead of `"hello-2.12.tar.gz"`.

**Fix:** Use proper Guix store path parsing:
```rust
let basename = drv_path.rsplit('/').next().unwrap();
let name = &basename[33..]; // skip "hash-" prefix
let name = name.strip_suffix(".drv").unwrap_or(name);
```

### 4.4 HIGH — Env var path rewriting is incomplete

Lines 137-138:
```rust
if env_var.value.starts_with("/gnu/store") {
    env_var.value = env_var.value.replace("/gnu/store", "/nix/store");
}
```

This only rewrites values that **start with** `/gnu/store`. Many env vars embed
store paths in the middle:
- `PATH=/gnu/store/...-coreutils/bin:/gnu/store/...-gcc/bin`
- `CPATH=/gnu/store/...-linux-headers/include:/gnu/store/...-glibc/include`
- Build scripts with `#!/gnu/store/...-bash/bin/bash` shebangs

The subsequent loop (lines 140-145) does map-based replacement which helps, but
only for paths already in `guix_to_nix_map`. The initial `/gnu/store` →
`/nix/store` prefix swap should apply unconditionally to any occurrence, not
just at the start.

**Fix:** Remove the `starts_with` guard — apply unconditionally:
```rust
env_var.value = env_var.value.replace("/gnu/store", "/nix/store");
```
Or better, do the map-based replacement first and then the prefix swap for
anything remaining.

### 4.5 MEDIUM — FOD handling requires pre-built Guix outputs

Lines 74-92: For fixed-output derivations with a builder other than
`builtin:fetchurl`, the splicer tries to copy existing outputs from
`/gnu/store` to `/nix/store`. If the output doesn't exist (i.e., the Guix
package hasn't been built yet), this silently falls through to the general
translation case, which will likely fail because the Guix builder won't work
under Nix.

For the hello target, the source tarball is a FOD fetched via
`builtin:download`, so this path applies to that. But intermediate build
artifacts (patches, configuration scripts) may also be FODs that need special
handling.

**Fix:** For FODs that are downloads, always use `builtin:fetchurl` or
`nixpkgs.fetchurl`. For FODs that are build products, they need to be built —
either by translating their builder or by substituting a Nix equivalent.

### 4.6 ~~MEDIUM~~ NOT AN ISSUE — Guix-specific builder (Guile)

~~Guix derivations use Guile as the builder. This was initially listed as a
blocker.~~

**This is not actually a problem.** The entire point of the bottom-up approach
is that *every* derivation gets translated, including Guile itself. If the
bootstrap boundary is handled correctly (§4.2), the chain propagates upward:

```
bootstrap seeds → mes → tcc → gcc-mesboot0 → ... → gcc → guile → hello
```

Each derivation's builder is just a store path that was produced by an earlier
derivation. By the time we reach `hello.drv`, its builder
(`/gnu/store/…-guile/bin/guile`) has already been translated to a Nix path
(`/nix/store/…-guile/bin/guile`) and built. The Guix build-side Scheme modules
(`(guix build utils)`, etc.) are just files listed in `input_srcs` — they get
path-rewritten and copied to the Nix store like any other source file.

**The real constraint this implies:** The bootstrap boundary (§4.2) must be
wide enough to catch *all* seed derivations, so the chain can build upward
without gaps. If even one intermediate derivation fails to translate, everything
above it (including Guile) breaks.

### 4.7 LOW — `add_to_nix_store` tmp file naming collision

Line 229:
```rust
let tmp_path = format!("/tmp/{}", name);
```

Multiple derivations processed concurrently (or with the same base name) would
clobber each other. Also, if the process is interrupted, orphan files remain in
`/tmp`.

**Fix:** Use `tempfile::NamedTempFile` or at minimum include a random suffix /
PID.

### 4.8 LOW — No support for multiple outputs

Nix derivations can have multiple outputs (`out`, `dev`, `lib`, `doc`, etc.).
Guix derivations also have multiple outputs. The splicer currently handles
`drv.outputs` as a list, but the env var rewriting (lines 166-183) only
matches by output name, which should work. However, the
`guix_to_nix_map` only stores the primary output mapping (line 88), missing
secondary outputs.

---

## 5. What Would a Working Hello Build Require?

Working backwards from "nix-store --realise produces `/nix/store/...-hello-2.12.2/bin/hello`":

### Step 1 — Fetch the source tarball
- Translate the `builtin:download` derivation for `hello-2.12.tar.gz`
- Must produce a valid `builtin:fetchurl` derivation with working URL
- **Status:** Partially implemented; broken by name extraction and single-URL

### Step 2 — Build the entire chain bottom-up
- The bottom-up approach means we translate *every* derivation from the
  bootstrap seeds up through Guile, gcc, glibc, make, coreutils, etc.
- Guix's builder (Guile) is just another package in this chain — it gets built
  along the way, so by the time we reach hello.drv, its builder path points to
  a working `/nix/store/…-guile/bin/guile`.
- The Guix build-side Scheme modules are `input_srcs` that get path-rewritten
  and added to the Nix store.
- **The key requirement:** the bootstrap boundary must be wide enough that
  every seed derivation is recognized and mapped to a Nix equivalent. Miss one
  and the chain breaks.
- **Status:** Not implemented — boundary detection too shallow (§4.2)

### Step 3 — Correct derivation registration
- The produced `.drv` must be at the correct Nix store path
- Must be registered with the Nix daemon as a valid derivation
- **Status:** Broken — `nix-store --add` doesn't work for derivations

### Step 4 — Correct path references
- Every store path in every env var, arg, script, and output must be rewritten
- **Status:** Partially implemented; env var rewriting has gaps

---

## 6. Proposed Approach for Hello

### Strategy: "Fix the bottom-up chain"

The existing bottom-up design is sound — the insight that Guile (the builder)
is just another package in the chain is correct. We don't need to replace the
builder or generate Nix expressions. We need to fix the three things preventing
the chain from working:

1. **Fix derivation registration (§4.1):** Use `nix derivation add` (JSON
   format on stdin) instead of `nix-store --add`. This ensures `.drv` files
   land at the correct store path and are recognized by the daemon.

2. **Widen the bootstrap boundary (§4.2):** The Guix bootstrap starts from
   binary seeds (`bootstrap-binaries-0`, `bootstrap-mescc-tools`,
   `bootstrap-mes`). These are pre-built tarballs with no source — they *are*
   the trust anchor. We need to:
   - Identify all seed/bootstrap derivations at the bottom of the DAG
   - Map each to its closest Nix equivalent (e.g. `bootstrap-mes` → a MES
     binary from nixpkgs, or simply provide the Guix bootstrap binaries as
     fixed-output derivations in Nix)
   - The simplest approach: treat the Guix bootstrap binaries as FODs — fetch
     them from the Guix substitute server and inject them as fixed-output
     derivations in Nix. Then everything above them builds organically.

3. **Fix `builtin:download` (§4.3):** Fix the name extraction, expand more
   mirror schemes, add URL fallback.

### The bootstrap binary question

The Guix bootstrap binaries (`bootstrap-mescc-tools`, `bootstrap-mes`,
`bootstrap-binaries-0`) are just tarballs. They can be:
- **(A)** Downloaded from `https://bordeaux.guix.gnu.org/` (the Guix
  substitute server) and injected into the Nix store as FODs.
- **(B)** Mapped to Nix equivalents (e.g. nixpkgs has `mes` and `tcc`
  packages).
- **(C)** Built from source in Nix (but this recapitulates Guix's own
  bootstrap, which is circular).

Option **(A)** is the most faithful to the "organic build" goal — we start from
the same seeds Guix uses, just hosted in the Nix store.

---

## 7. Implementation Roadmap

### Phase 0 — Fix what's there (make simple examples work)
- [ ] Fix derivation registration (use `nix derivation add` or correct store path)
- [ ] Fix name extraction from store paths
- [ ] Fix env var rewriting (remove `starts_with` guard)
- [ ] Add more mirror expansions
- [ ] Add URL fallback support

### Phase 1 — Make `builtin:download` FODs work end-to-end
- [ ] Translate a simple `builtin:download` drv and realise it
- [ ] Verify the fetched tarball hash matches

### Phase 2 — Define the splicing boundary for hello
- [ ] Enumerate the immediate dependencies of hello's top-level drv
- [ ] Create a mapping table: Guix package name → Nix store path
- [ ] Replace builder from Guile to bash + generated build script

### Phase 3 — Generate and build hello
- [ ] Produce the complete hello.drv with all rewrites
- [ ] Register it properly in the Nix store
- [ ] `nix-store --realise` it
- [ ] Verify `/nix/store/...-hello/bin/hello` runs

---

## 8. Reference: Guix vs Nix Derivation Formats

### ATerm format (both use this on disk)

```
Derive(
  [(output-name, output-path, hash-algo, hash), ...],
  [(input-drv-path, [output-names]), ...],
  [input-src-paths, ...],
  system,
  builder,
  [args, ...],
  [(env-key, env-value), ...]
)
```

Both Guix and Nix use essentially the same ATerm format for `.drv` files. The
key differences are:

| Aspect            | Guix                                 | Nix                                  |
|:------------------|:-------------------------------------|:-------------------------------------|
| Store prefix      | `/gnu/store/`                        | `/nix/store/`                        |
| Hash encoding     | `nix-base32` (same as Nix)           | `nix-base32`                         |
| Builder           | Often `/gnu/store/…/bin/guile`       | Usually `/nix/store/…/bin/bash`      |
| Build script      | Scheme (Guix build system modules)   | Bash (stdenv setup.sh)               |
| Fetch builtin     | `builtin:download`                   | `builtin:fetchurl`                   |
| Fetch env vars    | `url` (S-expr list)                  | `url` (single string), `name`        |
| System for fetch  | `x86_64-linux`                       | `builtin`                            |
| Path computation  | Same algorithm as Nix (forked)       | `text:sha256:<hash>:<refs>:/nix/store` |

### Nix derivation path computation

For **input-addressed** derivations:
```
hash("text:" + sha256(aterm_modulo) + ":" + input_store_paths + ":/nix/store:" + name)
```
Where `aterm_modulo` replaces output paths with "" and input drv paths with
their output hashes (recursively).

For **fixed-output** derivations:
```
hash("fixed:out:" + hash_algo + ":" + hash + ":/nix/store:" + name)
```
