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
paths), and the source fetcher (`builtin:download` vs a `fetchurl` FOD).

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

1. **`builtin:download` → pinned nixpkgs `fetchurl { urls = […]; }`.** Nix's
   own `builtin:fetchurl` can only take one URL and can't fall back, so each
   download becomes a fixed-output `pkgs.fetchurl` derivation carrying the
   *full* candidate list: Guix's content-addressed mirror first
   (`https://bordeaux.guix.gnu.org/file/<name>/sha256/<hash>` — it serves any
   source Guix's CI has seen, keyed by the hash we already have), then the
   upstream mirror declarations in deterministic order. The fallback happens
   at *build* time, inside the derivation, so mirror availability never
   changes the derivation identity and no URL is probed during translation.
2. **`builtin:git-download` → a substitute-first checkout fetcher.** Full and
   likely abbreviated SHA-1 commit IDs and existing `refs/...` values are
   preserved. Other revisions are expanded to `refs/tags/...`, including numeric
   tag names such as `20250605`. Unlike a download, a git source carries no
   mirror list to fall back on, so the fetcher first asks Guix's own build farms
   (bordeaux, then ci) for the checkout's nar — keyed by the `/gnu/store` hash
   the translated derivation already states — and clones upstream via
   `nix-prefetch-git` only when neither farm has it. Forges that rate-limit
   (savannah especially) therefore stop being a single point of failure. The
   result is a recursive-sha256 FOD either way, so which route ran never affects
   a store path.
3. **Sources are added** to the Nix store (text files get their `/gnu/store`
   references rewritten first).
4. **Every `/gnu/store` reference** — input derivations, builder, args, env — is
   rewritten to the already-translated `/nix/store` counterpart.
5. **Output paths are blanked** and the derivation is registered via
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

> **Note:** building the deep bootstrap (m4-boot0, hello) needs
> `--option filter-syscalls false` on the realise. Guix's early `tar` (gash) restores
> the setgid bit on directories inside some source tarballs, and Nix's seccomp
> filter otherwise blocks setuid/setgid `chmod` (it strips those bits from
> outputs regardless, so this only affects build-time temp dirs). Examples 1–4
> don't need it.

> **Note:** the realise also needs `/bin/sh` *removed* from the Nix sandbox, or
> some packages build differently than they do under `guix-daemon`. See
> [Sandbox parity](#sandbox-parity-nix-provides-binsh-guix-does-not).

Flags: `-v` for per-derivation logging, `--emit-nix <output.nix>` to generate a
standalone Nix expression (see below).

## Sandbox parity: Nix provides `/bin/sh`, Guix does not

Translating the derivation faithfully is not sufficient: the two daemons do not
present the same build environment, and the difference is not expressible in the
`.drv`. The one that has bitten us is `/bin/sh`.

`guix-daemon`'s container has no `/bin` at all. Nix's sandbox bind-mounts a
`/bin/sh` — the `sandbox-paths` default is compiled in, so per the Nix manual it
"may be empty or provide `/bin/sh` as a bind-mount of bash" depending on how the
Nix binary was built. nixpkgs builds Nix with
`-Dsandbox-shell=<busybox>/bin/busybox`, so on a nixpkgs-installed Nix `/bin/sh`
is present:

```console
❯ nix config show | grep sandbox-paths
sandbox-paths = /bin/sh=/nix/store/…-busybox-1.37.0/bin/busybox …
```

Guix packages are built with the assumption that no `/bin/sh` exists — that is
why Guix patches shebangs so aggressively — and a build step that quietly
depends on it will take a different path under Nix. So realise translated
derivations with the `/bin/sh` entry dropped from `sandbox-paths`:

```console
# keep any other entries your nix.conf adds (binfmt, /dev/nvidiactl?, …)
❯ nix-store --realise --option filter-syscalls false --option sandbox-paths '' …
```

Overriding `sandbox-paths` requires being a `trusted-user`, same as
`filter-syscalls`. Do **not** reach for `--option sandbox false` instead: that
exposes the *host's* `/bin/sh` along with the rest of the host filesystem, which
is strictly further from Guix's environment.

### Other known divergences

Not yet observed to change a build, but they exist and are equally invisible to
the `.drv`:

- `sandbox-build-dir` is `/build` under Nix, `/tmp/guix-build-<drv>-0` under
  Guix (different path *and* length).
- The translated derivations carry a few extra environment variables that Guix's
  do not (`name`, `builder`, `system`, `outputs`, `srcs`, `__phantom_deps`).

## `--emit-nix`: standalone Nix expressions

`guix-transfer` can emit a self-contained `.nix` file alongside the normal
translation. The file reconstructs every derivation in the graph as a
`builtins.derivation` call inside a single `let … in` block, with dependencies
wired via Nix string interpolation (so `inputDrvs`/`inputSrcs` are tracked
correctly).

```console
❯ ./target/release/guix-transfer --emit-nix /tmp/hello.nix /gnu/store/…-hello-2.12.2.drv
Emitted Nix expression: /tmp/hello.nix

❯ nix-build /tmp/hello.nix --no-out-link
/nix/store/…-hello-2.12.2
```

The generated expression can be imported from other Nix files:

```nix
let guixHello = import /tmp/hello.nix;
in derivation {
  name = "use-guix";
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [ "-c" "echo $(${guixHello}/bin/hello) from Nix > $out" ];
}
```

## Examples

A ladder of `.drv`-generating Scheme snippets, simplest first, lives in
[`examples/`](examples/). Run the whole suite with
[`examples/validate_all.sh`](examples/validate_all.sh).

| # | Example | Exercises | Realises under Nix |
|---|---------|-----------|:------------------:|
| 1 | `minimal` | raw `/bin/sh` derivation | ✅ → `Success` |
| 2 | `fod` | `builtin:download` → `fetchurl { urls = […]; }` | ✅ (downloads + hash-checks) |
| 3 | `dependencies` | a 2-level graph with an output reference | ✅ → `Captured: Shared Secret` |
| 4 | `bootstrap-seed` | `%bootstrap-guile`: executable seed downloads + a generated wrapper | ✅ **runs** `guile 2.0.9` under Nix |
| 5 | `m4-boot0` | the early bootstrap chain (140 derivations) | translates clean; realise = full mesboot compile |
| 6 | `hello` | the full hello DAG (228 derivations) | translates clean; realise rebuilds the world |
| 7 | `mixed` | Guix writes "hello", Nix appends " world" | ✅ cross-ecosystem composition |

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
