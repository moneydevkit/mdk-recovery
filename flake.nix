{
  description = "MDK Recovery";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/25.11";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      fenix,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        fenixPkgs = fenix.packages.${system};

        toolchain = fenixPkgs.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            toolchain
          ]
          ++ (with pkgs; [
            cargo-nextest
            just
            nixfmt-rfc-style
          ]);

          env = {
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
