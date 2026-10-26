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
    environment.systemPackages = [ cfg.package ];
    environment.etc."nanocode/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = toml.generate "nanocode-config.toml" cfg.settings;
    };
    environment.variables = lib.mkIf (cfg.settings != { }) {
      NANOCODE_CONFIG = "/etc/nanocode/config.toml";
    };
  };
}
