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
| Source fetcher  | `builtin:download` (mirror list) | `builtin:fetchurl` (one URL)  |
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
              ├─ builtin:download → builtin:fetchurl (Guix CA-mirror URL)   §5
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
| `mirrors.rs`  | `mirror://` expansion, URL extraction, host ranking (`--upstream` mode). |
| `net.rs`      | `curl` reachability probe (`--upstream` mode). |
| `json.rs`     | `Derivation` → Nix JSON derivation, **format version 4**. |
| `nixstore.rs` | Wrappers over `nix derivation add` / `nix derivation show` / `nix-store --add`. |
| `splicer.rs`  | Per-derivation translation, bottom-up; owns the guix→nix path map. |
| `main.rs`     | CLI (`-v`, `--upstream`); prints the final `.drv` to stdout. |

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

## 5. Source fetching: leveraging the Guix mirror

Guix's `builtin:download` is replaced with Nix's `builtin:fetchurl`. The
interesting decision is *which URL to give it*.

**The constraint.** `builtin:fetchurl` takes exactly one URL and cannot fall
back. Guix's `url` env, by contrast, is a Scheme list of mirror fallbacks, and
in practice those upstream URLs are unreliable for older sources:

- personal mirrors 404 (`lilypond.org/janneke`, `flashner.co.il`);
- a given file may live on only one host (the bootstrap guile tarball is on
  `alpha.gnu.org` but not `ftp.gnu.org`/`ftpmirror.gnu.org`);
- the i686 seed binaries are git-only and `cgit` rate-limits with flaky 301s.

Picking "the best" single upstream URL — by host reputation or even by live
probing — is therefore brittle.

**The fix.** Guix already runs a **content-addressed mirror** that serves *any*
source its CI has ever built, keyed purely by content hash — which is exactly
what the FOD record gives us. Its URL scheme (from Guix's own
`content-addressed-mirrors` definition) is:

```
https://bordeaux.guix.gnu.org/file/<name>/sha256/<nix-base32(hash)>
```

where `<name>` is the output's store name (e.g. `hello-2.12.2.tar.gz`, `tar`)
and the hash is the FOD's sha256 (the `r:` prefix, if any, denotes recursive
hashing and is stripped from the *value*). `guix-transfer` constructs this URL
directly from the derivation — no list, no probing, one URL that resolves.

This is faithful to the project's spirit: the sources come from Guix (its
substitute/CA infrastructure), and Nix builds everything above them. The
`mirror://` table, host ranking and probing still exist behind `--upstream`,
for when you specifically want to fetch from the original upstreams.

**Hash translation.** Guix's base16 hash + `sha256`/`r:sha256` algo (and the
`executable` download flag) map to Nix's SRI hash + `flat`/`nar` method:
`r:sha256` or `executable` ⇒ `nar`, otherwise `flat`. The download's Guix-only
env (`mirrors`, `disarchive-mirrors`, `content-addressed-mirrors`,
`impureEnvVars`, `preferLocalBuild`) is dropped; `executable` is preserved.

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
