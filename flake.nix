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

          runtimeDeps = with pkgs; [ openssl ];
          buildDeps = with pkgs; [ pkg-config ];
          devDeps = with pkgs; [ pkgs.ansible ];

          mkDevShell = rustc:
            pkgs.mkShell {
              shellHook = ''
                export RUST_SRC_PATH=${pkgs.rustPlatform.rustLibSrc}
              '';
              buildInputs = runtimeDeps;
              nativeBuildInputs = buildDeps ++ devDeps ++ [ rustc ];
            };
        in {
          _module.args.pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import inputs.rust-overlay) ];
          };
          packages.default = self'.packages.ci-bench-runner;
          devShells.default = self'.devShells.nightly;

          packages.ci-bench-runner = pkgs.rustPlatform.buildRustPackage {
            inherit (cargoToml.package) name version;
            src = crateDir;
            cargoLock = {
              lockFile = "${crateDir}/Cargo.lock";
              outputHashes = {
                "octocrab-0.31.2" =
                  "sha256-Gj3zevsE8o5/6hYryZGJ2XIL4yyveKdOud0YUZPhVys=";
              };
            };
            nativeBuildInputs = buildDeps;
            buildInputs = runtimeDeps;
            doCheck = false; # Some tests require platform certs.
          };

          # Nightly Rust dev env
          devShells.nightly = (mkDevShell (pkgs.rust-bin.selectLatestNightlyWith
            (toolchain: toolchain.default)));
          # Stable Rust dev env
          devShells.stable = (mkDevShell pkgs.rust-bin.stable.latest.default);
        };
    };
}
