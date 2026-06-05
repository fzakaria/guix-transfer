#!/usr/bin/env bash
# Validate the splicer end-to-end on the fast examples.
#
# For each example: generate the Guix .drv, translate it to Nix, realise it,
# and show the result. The bootstrap-seed example (4) exercises the download →
# fetchurl path and the input-addressed wrapper build; examples 5 (m4-boot0)
# and 6 (hello) translate fine but realising them rebuilds large parts of the
# world, so they are translate-only here unless REALISE_HEAVY=1.
set -uo pipefail
cd "$(dirname "$0")/.."

# Prefer a release build, fall back to debug.
BIN=$(ls target/release/guix-transfer target/debug/guix-transfer 2>/dev/null | head -1 || true)
[ -n "$BIN" ] && [ -x "$BIN" ] || { echo "Build first, e.g. devenv shell -- cargo build"; exit 1; }

FAST="examples/1-minimal.scm examples/2-fod.scm examples/3-dependencies.scm examples/4-bootstrap-seed.scm"
HEAVY="examples/5-m4-boot0.scm examples/6-hello.scm"

failures=0

run_one() {
    local scm="$1" realise="$2"
    echo ""
    echo "📍 $scm"
    local gdrv
    gdrv=$(guix repl "$scm" 2>/dev/null | tail -n 1)
    [ -z "$gdrv" ] && { echo "  ❌ could not generate Guix derivation"; return 1; }
    echo "  guix: $gdrv"
    local ndrv
    ndrv=$("$BIN" "$gdrv" 2>/tmp/gt.err)
    [ -z "$ndrv" ] && { echo "  ❌ translation failed:"; sed 's/^/     /' /tmp/gt.err; return 1; }
    echo "  nix:  $ndrv"
    if [ "$realise" != "1" ]; then
        echo "  ⏭️  translate-only (set REALISE_HEAVY=1 to build)"
        return 0
    fi
    # filter-syscalls=false lets the early bootstrap's gash tar restore setgid
    # dirs from source tarballs (Nix clears setuid/setgid in outputs anyway).
    local out
    out=$(nix-store --realise --option filter-syscalls false "$ndrv" 2>/tmp/gr.err)
    [ -z "$out" ] && { echo "  ❌ realise failed:"; tail -n 5 /tmp/gr.err | sed 's/^/     /'; return 1; }
    echo "  ✅ $out"
    if [ -f "$out" ] && file -b "$out" | grep -qi text; then
        echo "     $(head -c 80 "$out")"
    fi
}

echo "--- 🏗️  Guix→Nix splicer validation ---"
for scm in $FAST; do run_one "$scm" 1 || failures=$((failures + 1)); done
for scm in $HEAVY; do run_one "$scm" "${REALISE_HEAVY:-0}" || failures=$((failures + 1)); done
echo ""
if [ "$failures" -ne 0 ]; then
    echo "--- ❌ $failures example(s) failed ---"
    exit 1
fi
echo "--- 🏁 all good ---"
