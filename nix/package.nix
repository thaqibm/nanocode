{
  lib,
  rustPlatform,
  makeWrapper,
  pkg-config,
  attr,
  coreutils,
  findutils,
  git,
  gnugrep,
  gnused,
  util-linux,
}:
let
  manifest = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "nanocode";
  inherit (manifest.workspace.package) version;

  src = lib.cleanSourceWith {
    src = lib.cleanSource ../.;
    filter =
      path: _type:
      !(builtins.elem (baseNameOf path) [
        "target"
        "output"
        "result"
        "flake.nix"
        "flake.lock"
        "nix"
      ]);
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    allowBuiltinFetchGit = true;
  };

  postPatch = ''
    rm -f .cargo/config.toml
  '';

  cargoBuildFlags = [
    "-p"
    "nanocode-cli"
  ];
  doCheck = false;

  nativeBuildInputs = [
    makeWrapper
    pkg-config
    rustPlatform.bindgenHook
  ];

  postInstall = ''
    wrapProgram $out/bin/nanocode \
      --suffix PATH : ${
        lib.makeBinPath [
          attr
          coreutils
          findutils
          git
          gnugrep
          gnused
          util-linux
        ]
      }
  '';

  meta = {
    description = "Terminal coding agent for Codex, Claude and OpenAI-compatible open models";
    homepage = "https://github.com/thaqibm/nanocode";
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = "nanocode";
    platforms = lib.platforms.linux;
  };
}
