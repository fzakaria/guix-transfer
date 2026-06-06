#!/usr/bin/env bash
# Example 7: Mixed Guix + Nix derivation
#
# Demonstrates cross-ecosystem composition:
#   1. A Guix derivation writes "hello" to its output.
#   2. guix-transfer translates it into a Nix derivation.
#   3. nix-store --realise builds the translated derivation.
#   4. A native Nix expression references the Guix output and
#      appends " world" — built via nix-build.
#   5. Result: "hello world" — half Guix, half Nix.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=$(ls target/release/guix-transfer target/debug/guix-transfer 2>/dev/null | head -1 || true)
[ -n "$BIN" ] && [ -x "$BIN" ] || { echo "Build first: cargo build --release"; exit 1; }

echo "=== Step 1: Create Guix derivation ==="
GUIX_DRV=$(guix repl examples/7-mixed.scm 2>/dev/null | tail -n 1)
echo "  Guix drv: $GUIX_DRV"

echo ""
echo "=== Step 2: Translate to Nix ==="
NIX_DRV=$("$BIN" "$GUIX_DRV" 2>/dev/null)
echo "  Nix drv:  $NIX_DRV"

echo ""
echo "=== Step 3: Realise the translated Guix derivation ==="
GUIX_OUT=$(nix-store --realise --option filter-syscalls false "$NIX_DRV")
echo "  Output:   $GUIX_OUT"
echo "  Content:  $(cat "$GUIX_OUT")"

echo ""
echo "=== Step 4: Build a native Nix expression on top ==="
# Generate a Nix expression that references the Guix output as a dependency.
# The store path is interpolated — Nix tracks it as an input automatically.
NIX_EXPR=$(mktemp --suffix=.nix)
cat > "$NIX_EXPR" <<EOF
let
  guixHello = $GUIX_OUT;
in
derivation {
  name = "nix-hello-world";
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [ "-c" "echo -n \"\$(cat \${guixHello}) world\" > \$out" ];
  PATH = "/bin";
}
EOF
echo "  Generated: $NIX_EXPR"
cat "$NIX_EXPR" | sed 's/^/    /'

echo ""
MIXED_OUT=$(nix-build --no-out-link --option filter-syscalls false "$NIX_EXPR")
echo "  Output:  $MIXED_OUT"
echo "  Content: $(cat "$MIXED_OUT")"

rm -f "$NIX_EXPR"

echo ""
EXPECTED="hello world"
ACTUAL=$(cat "$MIXED_OUT")
if [ "$ACTUAL" = "$EXPECTED" ]; then
    echo "✅ Success: Guix wrote 'hello', Nix appended ' world' → '$ACTUAL'"
else
    echo "❌ Expected '$EXPECTED', got '$ACTUAL'"
    exit 1
fi
