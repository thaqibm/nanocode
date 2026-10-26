{ self, ... }:
{
  perSystem =
    { pkgs, ... }:
    {
      packages = rec {
        nanocode = pkgs.callPackage ./package.nix { };
        default = nanocode;
      };
      formatter = pkgs.nixfmt-rfc-style;
    };

  flake = {
    overlays.default = final: _prev: {
      nanocode = final.callPackage ./package.nix { };
    };

    modules.nixos.nanocode = import ./nixos.nix self;
    modules.homeManager.nanocode = import ./home-manager.nix self;

    nixosModules.default = self.modules.nixos.nanocode;
    homeManagerModules.default = self.modules.homeManager.nanocode;
  };
}
