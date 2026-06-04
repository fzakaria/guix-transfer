#!/usr/bin/env bash

echo "--- 🏗️ Starting Validation Suite ---"

for scm in examples/*.scm; do
    echo ""
    echo "📍 Testing: $scm"
    
    # 1. Generate the Guix derivation
    # We use 'guix repl' to ensure Guix modules are available
    # We use tail -n 1 to catch the last line in case of REPL noise
    GUIX_DRV=$(guix repl "$scm" 2>/dev/null | tail -n 1)
    
    if [ -z "$GUIX_DRV" ]; then
        echo "❌ Error: Failed to generate Guix derivation for $scm"
        continue
    fi
    echo "Generated Guix .drv: $GUIX_DRV"
    
    # 2. Run the Splicer
    # Capture the output and extract the Nix path
    # We use 'nix-shell' inside the script if needed, but assuming user is in one.
    SPLICER_OUT=$(cargo run --quiet -- "$GUIX_DRV")
    NIX_DRV=$(echo "$SPLICER_OUT" | grep "Final Nix derivation:" | awk '{print $NF}')
    
    if [ -z "$NIX_DRV" ]; then
        echo "❌ Error: Splicer failed to produce a Nix derivation"
        echo "   Splicer output: $SPLICER_OUT"
        continue
    fi
    echo "Translated Nix .drv: $NIX_DRV"
    
    # 3. Realize the result
    echo "Realizing in Nix..."
    # We use --no-out-link to avoid cluttering the workspace
    RESULT=$(nix-store --realise "$NIX_DRV" --quiet)
    
    if [ -z "$RESULT" ]; then
        echo "❌ Error: Nix realization failed"
        continue
    fi

    echo "✅ Success! Output: $RESULT"
    if [ -f "$RESULT" ]; then
        echo "   Content: $(cat "$RESULT" | head -n 1)"
    elif [ -d "$RESULT" ]; then
        echo "   Directory contents: $(ls "$RESULT" | head -n 5)"
    fi
done

echo ""
echo "--- 🏁 All examples validated! ---"
