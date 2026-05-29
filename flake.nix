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
        inherit (pkgs) lib;
        isLinux = pkgs.stdenv.isLinux;

        pkgsUnstable = nixpkgs-unstable.legacyPackages.${system};
        fenixPkgs = fenix.packages.${system};

        # Native toolchain — used by `just check`, the dev shell, and
        # the macOS release builds (darwin runners build their own
        # arch natively, no cross-compilation).
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

        nativeBin = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            doCheck = false;
          }
        );

        # Linux release builds statically link against musl so a
        # single binary runs on any glibc (or no libc). Darwin
        # builds run on darwin runners and use the default dynamic
        # linkage — every macOS host has the system libs already.
        muslTarget =
          {
            x86_64-linux = "x86_64-unknown-linux-musl";
            aarch64-linux = "aarch64-unknown-linux-musl";
          }
          .${system} or null;

        muslCrossPkgs =
          {
            x86_64-linux = pkgs.pkgsCross.musl64;
            aarch64-linux = pkgs.pkgsCross.aarch64-multiplatform-musl;
          }
          .${system} or null;

        muslToolchain = lib.optionals isLinux [
          fenixPkgs.targets.${muslTarget}.stable.rust-std
        ];

        staticToolchain = fenixPkgs.combine (
          [
            (fenixPkgs.stable.withComponents [
              "cargo"
              "rustc"
            ])
          ]
          ++ muslToolchain
        );

        staticCraneLib = (crane.mkLib pkgs).overrideToolchain staticToolchain;

        muslTargetUnderscored = builtins.replaceStrings [ "-" ] [ "_" ] (
          if muslTarget != null then muslTarget else ""
        );
        muslTargetUpperUnderscored = lib.toUpper muslTargetUnderscored;

        staticArgs = lib.optionalAttrs isLinux (
          commonArgs
          // {
            CARGO_BUILD_TARGET = muslTarget;
            CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
            "CC_${muslTargetUnderscored}" =
              "${muslCrossPkgs.stdenv.cc}/bin/${muslCrossPkgs.stdenv.cc.targetPrefix}cc";
            "CARGO_TARGET_${muslTargetUpperUnderscored}_LINKER" =
              "${muslCrossPkgs.stdenv.cc}/bin/${muslCrossPkgs.stdenv.cc.targetPrefix}cc";
            nativeBuildInputs = [ muslCrossPkgs.stdenv.cc ];
          }
        );

        staticCargoArtifacts = lib.optionalAttrs isLinux (staticCraneLib.buildDepsOnly staticArgs);

        staticBin = lib.optionalAttrs isLinux (
          staticCraneLib.buildPackage (
            staticArgs
            // {
              cargoArtifacts = staticCargoArtifacts;
              doCheck = false;
            }
          )
        );
      in
      {
        packages = {
          default = nativeBin;

          # Uniform target for the release workflow: a static musl
          # binary on linux, a native dynamic binary on darwin. The
          # workflow runs `nix build .#release` on every runner and
          # always finds the right thing in `result/bin/mdk-recovery`.
          release = if isLinux then staticBin else nativeBin;
        }
        // lib.optionalAttrs isLinux {
          static = staticBin;
        };

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

        devShells = {
          default = pkgs.mkShell {
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

          # Slim shell for the release job.
          release = pkgs.mkShell {
            packages = with pkgs; [
              nodejs_24
              minisign
              jq
            ];
          };
        };
      }
    );
}
