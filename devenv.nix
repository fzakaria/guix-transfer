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
  };

  # See full reference at https://devenv.sh/reference/options/
}
