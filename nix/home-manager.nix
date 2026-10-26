self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.nanocode;
  toml = pkgs.formats.toml { };
in
{
  options.programs.nanocode = import ./options.nix {
    inherit self lib pkgs toml;
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    home.file.".nanocode/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = toml.generate "nanocode-config.toml" cfg.settings;
    };
  };
}
