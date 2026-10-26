{
  description = "nanocode: terminal coding agent for Codex, Claude and OpenAI-compatible open models";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        flake-parts.flakeModules.modules
        ./nix/flake-module.nix
      ];
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
    };
}
