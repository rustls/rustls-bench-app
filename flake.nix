{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    systems.url = "github:nix-systems/default";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = import inputs.systems;
      perSystem = { config, self', pkgs, lib, system, ... }:
        let
          crateDir = ./ci-bench-runner;
          cargoToml =
            builtins.fromTOML (builtins.readFile "${crateDir}/Cargo.toml");

          devDeps = with pkgs; [ ansible ansible-lint valgrind ];

          mkDevShell = rustc:
            pkgs.mkShell {
              shellHook = ''
                export RUST_SRC_PATH=${pkgs.rustPlatform.rustLibSrc}
                export LC_ALL="C.UTF-8"
              '';
              nativeBuildInputs = devDeps ++ [ pkgs.cmake rustc ];
            };
        in {
          _module.args.pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import inputs.rust-overlay) ];
          };
          devShells.default = self'.devShells.nightly;

          # Nightly Rust dev env
          devShells.nightly = (mkDevShell (pkgs.rust-bin.selectLatestNightlyWith
            (toolchain: toolchain.default)));
          # Stable Rust dev env
          devShells.stable = (mkDevShell pkgs.rust-bin.stable.latest.default);
        };
    };
}
