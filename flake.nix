{
  description = "MDK Recovery";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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

        # Cross-compile x86_64-apple-darwin from the aarch64-darwin
        # runner. macOS hosts have a universal SDK and clang that
        # targets either arch, so no extra linker config is needed —
        # just the x86_64 rust-std combined with the native toolchain.
        isAarch64Darwin = system == "aarch64-darwin";

        darwinX64Toolchain = lib.optionalAttrs isAarch64Darwin (
          fenixPkgs.combine [
            (fenixPkgs.stable.withComponents [
              "cargo"
              "rustc"
            ])
            fenixPkgs.targets.x86_64-apple-darwin.stable.rust-std
          ]
        );

        darwinX64CraneLib = lib.optionalAttrs isAarch64Darwin (
          (crane.mkLib pkgs).overrideToolchain darwinX64Toolchain
        );

        darwinX64Args = lib.optionalAttrs isAarch64Darwin (
          commonArgs
          // {
            CARGO_BUILD_TARGET = "x86_64-apple-darwin";
          }
        );

        darwinX64CargoArtifacts = lib.optionalAttrs isAarch64Darwin (
          darwinX64CraneLib.buildDepsOnly darwinX64Args
        );

        darwinX64Bin = lib.optionalAttrs isAarch64Darwin (
          darwinX64CraneLib.buildPackage (
            darwinX64Args
            // {
              cargoArtifacts = darwinX64CargoArtifacts;
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
        }
        // lib.optionalAttrs isAarch64Darwin {
          darwin-x64 = darwinX64Bin;
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

          # Lint the GitHub Actions workflows. shellcheck on PATH means
          # actionlint also checks the embedded `run:` shell scripts.
          actionlint =
            pkgs.runCommandLocal "actionlint"
              {
                nativeBuildInputs = [
                  pkgs.actionlint
                  pkgs.shellcheck
                ];
              }
              ''
                cp -r ${./.github} .github
                # Pass files explicitly: outside a git checkout actionlint
                # can't auto-detect the project root.
                actionlint .github/workflows/*.yml
                touch $out
              '';

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
              actionlint
              cargo-nextest
              just
              nixfmt-rfc-style
              shellcheck
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
              jq
            ];
          };
        };
      }
    );
}
