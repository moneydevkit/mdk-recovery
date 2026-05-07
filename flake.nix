{
  description = "MDK Recovery";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/25.11";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    # Pinned for bitcoind 29.0 to match `corepc-node`'s `29_0` feature.
    # Bump in lockstep with the corepc-node feature flag in Cargo.toml.
    nixpkgs-unstable.url = "github:nixos/nixpkgs/e6f23dc08d3624daab7094b701aa3954923c6bbb";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      fenix,
      crane,
      nixpkgs-unstable,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        pkgsUnstable = nixpkgs-unstable.legacyPackages.${system};
        fenixPkgs = fenix.packages.${system};

        toolchain = fenixPkgs.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          strictDeps = true;
          # Pin the bitcoind binary `corepc-node` will spawn so the
          # nix sandbox doesn't try to pull one down at test time.
          BITCOIND_EXE = "${pkgsUnstable.bitcoind}/bin/bitcoind";
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in
      {
        packages.default = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            doCheck = false;
          }
        );

        checks = {
          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          fmt = craneLib.cargoFmt { inherit src; };

          test = craneLib.cargoNextest (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoNextestExtraArgs = "--no-tests=pass";
            }
          );
        };

        devShells.default = pkgs.mkShell {
          packages = [
            toolchain
            pkgsUnstable.bitcoind
          ]
          ++ (with pkgs; [
            cargo-nextest
            just
            nixfmt-rfc-style
          ]);

          env = {
            BITCOIND_EXE = "${pkgsUnstable.bitcoind}/bin/bitcoind";
            NIX_SYSTEM = system;
          };

          shellHook = ''
            echo "================================================================================"
            echo "MDK Recovery Development Environment"

            echo "Configuring Project..."
            git config core.hooksPath .githooks

            echo "Development Environment Ready."
            echo "================================================================================"
          '';
        };
      }
    );
}
