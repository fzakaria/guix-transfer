#!/usr/bin/env bash
# Focused integration test for build-time URL fallback.
#
# Uses only loopback HTTP fixtures. `file://` cannot be read inside Nix's
# sandbox, while the fetchurl builder can reach the local server.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=${BIN:-target/release/guix-transfer}
[ -x "$BIN" ] || { echo "build $BIN first" >&2; exit 1; }
NIXPKGS_REV=3e41b24abd260e8f71dbe2f5737d24122f972158
work=$(mktemp -d)
server=
cleanup() {
    [ -z "$server" ] || kill "$server" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

mkdir -p "$work/http"
printf 'fallback fixture\n' >"$work/http/payload"
printf 'wrong fixture\n' >"$work/http/wrong-first"
printf '#!/bin/sh\necho executable fixture\n' >"$work/http/executable"
chmod 0644 "$work/http/executable"
port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$work/http" >"$work/http.log" 2>&1 &
server=$!
sleep 1
base="http://127.0.0.1:$port"
# Reserve and close a distinct port, guaranteeing a real loopback connection
# refusal rather than an HTTP response from the fixture server.
refused_port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)

flat_hash=$(guix hash -H sha256 -f base16 "$work/http/payload")
cp "$work/http/payload" "$work/recursive"
chmod 0444 "$work/recursive"
recursive_hash=$(guix hash -H sha256 -f base16 -S nar "$work/recursive")
cp "$work/http/executable" "$work/executable-hash"
chmod 0555 "$work/executable-hash"
executable_hash=$(guix hash -H sha256 -f base16 -S nar "$work/executable-hash")

make_drv() {
    local name=$1 hash=$2 algo=$3 executable=$4 urls=$5
    local hash_algo=${algo#r:} recursive= executable_env=
    if [ "$algo" != "$hash_algo" ] || [ -n "$executable" ]; then
        recursive=' #:recursive? #t'
    fi
    if [ -n "$executable" ]; then
        executable_env=' (cons "executable" "1")'
    fi
    local scm="$work/$name.scm"
    cat >"$scm" <<EOF
(use-modules (guix derivations) (guix store) (rnrs bytevectors) (guix base16))
(with-store %store
  (let ((drv (derivation %store "$name" "builtin:download" '()
                         #:env-vars (list (cons "url" (object->string '$urls))$executable_env)
                         #:hash (base16-string->bytevector "$hash")
                         #:hash-algo '$hash_algo$recursive
                         #:system "x86_64-linux")))
    (format #t "~a\\n" (derivation-file-name drv))))
EOF
    guix repl "$scm" | tail -n 1
}

# Build a Guix parent that explicitly consumes the same local fixture source.
# This proves source context/inputDrvs survive direct and both emitted forms.
make_parent_drv() {
    local source_name=$1 hash=$2 urls=$3
    local scm="$work/$source_name-parent.scm"
    cat >"$scm" <<EOF
(use-modules (guix derivations) (guix store) (rnrs bytevectors) (guix base16))
(with-store %store
  (let* ((source (derivation %store "$source_name" "builtin:download" '()
                              #:env-vars (list (cons "url" (object->string '$urls)))
                              #:hash (base16-string->bytevector "$hash")
                              #:hash-algo 'sha256
                              #:system "x86_64-linux"))
         (parent (derivation %store "$source_name-parent" "/bin/sh"
                             (list "-c" (format #f "test -f ~a; : > \$out" (derivation->output-path source)))
                             #:env-vars '(("PATH" . "/bin"))
                             #:inputs (list (list source "out"))
                             #:system "x86_64-linux")))
    (format #t "~a\\n" (derivation-file-name parent))))
EOF
    guix repl "$scm" | tail -n 1
}

translate() {
    "$BIN" --upstream --nixpkgs "$NIXPKGS_REV" "$1"
}

out_path() {
    nix-store --query --outputs "$1" | head -n 1
}

assert_parent_uses_source() {
    local parent=$1 source=$2
    # `nix derivation show` abbreviates input keys relative to /nix/store.
    nix derivation show "$parent" | grep -F "$(basename "$source")" >/dev/null || {
        echo "parent $parent does not consume source $source" >&2
        exit 1
    }
}

# Flat fallback: a 404 first URL falls through to the fixture. The same source
# identity is observed before and after build, proving availability did not
# participate in translation-time identity.
flat_urls="(\"$base/missing\" \"$base/payload\")"
flat_drv=$(make_drv fallback-flat "$flat_hash" sha256 '' "$flat_urls")
direct_source=$(translate "$flat_drv")
direct_source_out=$(out_path "$direct_source")
pre=$direct_source
nix-store --realise "$direct_source" >/dev/null
post=$(translate "$flat_drv")
[ "$pre" = "$post" ]

# Explicitly compare the standalone source's .drv and output identities in the
# direct and single-file forms; the parent check below proves it is the same
# source consumed by the single-file consumer expression.
source_emit="$work/source.nix"
"$BIN" --upstream --nixpkgs "$NIXPKGS_REV" --emit-nix "$source_emit" "$flat_drv" >/dev/null
single_source=$(nix eval --impure --raw --expr "(import $source_emit).drvPath")
single_source_out=$(nix eval --impure --raw --expr "(import $source_emit).outPath")
[ "$direct_source" = "$single_source" ]
[ "$direct_source_out" = "$single_source_out" ]

# A local Guix parent consuming that source must retain both source and parent
# identities through direct translation, --emit-nix, and --emit-nix-dir.
parent_drv=$(make_parent_drv fallback-flat "$flat_hash" "$flat_urls")
direct_parent=$(translate "$parent_drv")
direct_parent_out=$(out_path "$direct_parent")
assert_parent_uses_source "$direct_parent" "$direct_source"

emit="$work/fallback.nix"
emit_dir="$work/emitted"
"$BIN" --upstream --nixpkgs "$NIXPKGS_REV" --emit-nix "$emit" --emit-nix-dir "$emit_dir" "$parent_drv" >/dev/null
single_parent=$(nix eval --impure --raw --expr "(import $emit).drvPath")
single_parent_out=$(nix eval --impure --raw --expr "(import $emit).outPath")
[ "$direct_parent" = "$single_parent" ]
[ "$direct_parent_out" = "$single_parent_out" ]
assert_parent_uses_source "$single_parent" "$direct_source"

source_dir_nix="$emit_dir/store/$(basename "$direct_source_out").nix"
dir_source=$(nix eval --impure --raw --expr "(import $source_dir_nix).drvPath")
dir_source_out=$(nix eval --impure --raw --expr "(import $source_dir_nix).outPath")
[ "$direct_source" = "$dir_source" ]
[ "$direct_source_out" = "$dir_source_out" ]
parent_dir_nix="$emit_dir/store/$(basename "$direct_parent" .drv).nix"
dir_parent=$(nix eval --impure --raw --expr "(import $parent_dir_nix).drvPath")
dir_parent_out=$(nix eval --impure --raw --expr "(import $parent_dir_nix).outPath")
[ "$direct_parent" = "$dir_parent" ]
[ "$direct_parent_out" = "$dir_parent_out" ]
assert_parent_uses_source "$dir_parent" "$direct_source"

nix-store --realise "$direct_parent" >/dev/null
nix build --impure --no-link --expr "import $emit"
nix build --impure --no-link --expr "import $parent_dir_nix"

# Recursive and executable modes are represented by the pinned helper's
# recursiveHash/executable attributes and realise from the same local fixture.
recursive_drv=$(make_drv fallback-recursive "$recursive_hash" r:sha256 '' "(\"$base/missing-recursive\" \"$base/payload\")")
nix-store --realise "$(translate "$recursive_drv")" >/dev/null
executable_drv=$(make_drv fallback-executable "$executable_hash" sha256 1 "(\"$base/missing-executable\" \"$base/executable\")")
executable_out=$(nix-store --realise "$(translate "$executable_drv")")
test -x "$executable_out"

# Default mode prepends the CA candidate for every supported hash mode. This
# instantiates only; it does not contact the public CA endpoint.
for drv in "$flat_drv" "$recursive_drv" "$executable_drv"; do
    default_emit="$work/default-$(basename "$drv").nix"
    "$BIN" --nixpkgs "$NIXPKGS_REV" --emit-nix "$default_emit" "$drv" >/dev/null
    grep -F 'https://bordeaux.guix.gnu.org/file/' "$default_emit" >/dev/null
done

# A real transport refusal falls through to the working loopback candidate.
# The request log proves the fallback reached the second URL.
: >"$work/http.log"
refused_drv=$(make_drv "fallback-refused-$refused_port" "$flat_hash" sha256 '' "(\"http://127.0.0.1:$refused_port/refused\" \"$base/payload\")")
nix-store --realise "$(translate "$refused_drv")" >/dev/null
grep -F 'GET /payload ' "$work/http.log" >/dev/null

# All transport/HTTP failures name every attempted URL.
missing_drv=$(make_drv fallback-all-missing "$flat_hash" sha256 '' "(\"$base/absent-one\" \"$base/absent-two\")")
if nix-store --realise "$(translate "$missing_drv")" >"$work/missing.out" 2>"$work/missing.err"; then
    echo "all-missing fetch unexpectedly succeeded" >&2
    exit 1
fi
grep -F "$base/absent-one" "$work/missing.err"
grep -F "$base/absent-two" "$work/missing.err"

# A successful wrong transfer is authoritative: Nix reports its FOD hash
# mismatch and must not fetch the later URL that would have matched.
: >"$work/http.log"
wrong_drv=$(make_drv fallback-wrong-first "$flat_hash" sha256 '' "(\"$base/wrong-first\" \"$base/payload\")")
if nix-store --realise "$(translate "$wrong_drv")" >"$work/wrong.out" 2>"$work/wrong.err"; then
    echo "wrong-first fetch unexpectedly succeeded" >&2
    exit 1
fi
grep -Eqi 'hash mismatch|got:.*sha256' "$work/wrong.err"
grep -F 'GET /wrong-first ' "$work/http.log" >/dev/null
if grep -F 'GET /payload ' "$work/http.log" >/dev/null; then
    echo "wrong hash incorrectly fetched the later candidate" >&2
    exit 1
fi

echo "fetchurl fallback integration passed"
