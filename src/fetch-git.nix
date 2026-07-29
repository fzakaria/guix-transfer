# Fetcher for translated Guix git-fetch sources.
#
# Guix's own build farms already hold every checkout translated here, so the
# upstream clone is the last resort rather than the first. A git-fetch source
# has no mirror list to fall back on the way a url-fetch source does, and
# savannah rate-limits and times out under load, which strands every package
# whose source lives there.
#
# Both routes land on the identical store path: the output is a
# recursive-sha256 fixed-output derivation, so its path is fixed by (name,
# hash) alone and how the bytes arrived is invisible to every consumer. That
# also means switching a checkout from one route to the other rebuilds nothing.
#
# getFlake is let-bound outside the lambda so one evaluator pays the nixpkgs
# evaluation once, however many checkouts it instantiates (builtins.getFlake
# is not memoized across calls).
let
  pkgs = (builtins.getFlake "@NIXPKGS_FLAKE@").legacyPackages.x86_64-linux;

  # Guix substitute servers, tried in this order. bordeaux (Guix Build
  # Coordinator) retains sources longest; ci (Cuirass) is the older farm and
  # advertises gzip alongside lzip.
  guixSubstituterUrls = [
    "https://bordeaux.guix.gnu.org"
    "https://ci.guix.gnu.org"
  ];
in
{
  url,
  rev,
  hash,
  name,
  # Hash part of Guix's own store path for this checkout, recorded from the
  # translated derivation: the key its nar is filed under on the farms.
  guixHash,
  fetchSubmodules ? false,
}:
pkgs.stdenvNoCC.mkDerivation {
  inherit
    name
    url
    rev
    guixHash
    ;

  outputHashAlgo = "sha256";
  outputHashMode = "recursive";
  outputHash = hash;

  nativeBuildInputs = [
    pkgs.curl
    pkgs.gzip
    pkgs.lzip
    pkgs.zstd
    # `nix-store --restore` deserializes a nar; it touches no store and no daemon.
    pkgs.nix
    # The same script pkgs.fetchgit drives, so the clone fallback matches it exactly.
    pkgs.nix-prefetch-git
    pkgs.cacert
  ];

  # Only scalars reach the builder as environment variables, so the server list
  # and the submodule flag are rendered to strings here.
  guixSubstituters = builtins.concatStringsSep " " guixSubstituterUrls;
  submodulesFlag = if fetchSubmodules then "--fetch-submodules" else "";

  # curl reads SSL_CERT_FILE; nix-prefetch-git derives git's GIT_SSL_CAINFO from
  # NIX_GIT_SSL_CAINFO or NIX_SSL_CERT_FILE, so both are set rather than left to
  # whatever the daemon happens to expose. NIX_GIT_SSL_CAINFO stays impure for
  # the same reason pkgs.fetchgit keeps it so: a caller may need to override it.
  SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
  NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
  impureEnvVars = pkgs.lib.fetchers.proxyImpureEnvVars ++ [
    "GIT_PROXY_COMMAND"
    "NIX_GIT_SSL_CAINFO"
    "SOCKS_SERVER"
  ];
  preferLocalBuild = true;

  buildCommand = ''
    # nix-store writes nothing outside $out, but the CLI wants a writable HOME.
    export HOME="$NIX_BUILD_TOP"

    for substituter in $guixSubstituters; do
      narinfo=$(curl --silent --show-error --fail --location --max-time 60 \
        "$substituter/$guixHash.narinfo") || continue

      # A narinfo may advertise several encodings; the first URL/Compression
      # pair is the server's own preference.
      narPath=$(printf '%s\n' "$narinfo" | sed -n 's/^URL: //p' | head -n1)
      compression=$(printf '%s\n' "$narinfo" | sed -n 's/^Compression: //p' | head -n1)
      [ -n "$narPath" ] || continue

      curl --silent --show-error --fail --location --max-time 1800 \
        --output nar.compressed "$substituter/$narPath" || continue

      case "$compression" in
        lzip) lzip --decompress --stdout nar.compressed > nar ;;
        gzip) gzip --decompress --stdout nar.compressed > nar ;;
        zstd) zstd --decompress --stdout nar.compressed > nar ;;
        none) mv nar.compressed nar ;;
        *)
          echo "$name: unknown nar compression '$compression' from $substituter" >&2
          continue
          ;;
      esac

      # The fixed output hash is what validates these bytes, so a stale or
      # hostile substitute fails the build rather than poisoning the store.
      if nix-store --restore "$out" < nar; then
        echo "$name: restored from $substituter"
        exit 0
      fi

      echo "$name: nar from $substituter did not restore" >&2
      rm -rf "$out" nar nar.compressed
    done

    # Neither farm has it: clone upstream exactly as pkgs.fetchgit would.
    echo "$name: no Guix substitute, cloning $url" >&2
    nix-prefetch-git --builder --url "$url" --out "$out" --rev "$rev" \
      --name "$name" $submodulesFlag
  '';
}
