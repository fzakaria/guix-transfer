{
  pkgs,
  lib,
  config,
  ...
}:
{
  # https://devenv.sh/languages/
  languages.rust = {
    enable = true;
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
  };

  # `devenv test` (run by CI): format check, lint, and the store-independent
  # unit tests. The end-to-end examples need `guix` + a Nix daemon and so are
  # not run here.
  enterTest = ''
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
  '';

  # See full reference at https://devenv.sh/reference/options/
}
