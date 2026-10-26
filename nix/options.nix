{
  self,
  lib,
  pkgs,
  toml,
}:
{
  enable = lib.mkEnableOption "nanocode, the terminal coding agent";

  package = lib.mkOption {
    type = lib.types.package;
    default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    defaultText = lib.literalExpression "inputs.nanocode.packages.\${system}.default";
    description = "The nanocode package to install.";
  };

  settings = lib.mkOption {
    inherit (toml) type;
    default = { };
    description = ''
      nanocode's config.toml as a Nix attribute set. Keep API keys out of the
      Nix store: use a provider's `api_key_file` (for example a sops-nix or
      agenix secret) or `api_key_env` instead of `api_key`.
    '';
    example = lib.literalExpression ''
      {
        model = "kimi-k3";
        permissions = "review-edits";
        batch = true;
        diff_viewer = "delta";
        diff_tool = "nvim -d";
        diff_preview_lines = 10;
        providers.baseten = {
          base_url = "https://inference.baseten.co/v1";
          api_key_file = "/run/secrets/baseten";
          models.kimi-k3 = "moonshotai/Kimi-K3";
        };
      }
    '';
  };
}
