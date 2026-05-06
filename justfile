default:
    @just --list

system := env("NIX_SYSTEM")

# Run all checks (fmt, clippy, unit tests)
check:
    nix flake check

# Format code
fmt:
    cargo fmt
    nixfmt flake.nix

# Check formatting
fmt-check:
    nix build .#checks.{{system}}.fmt

# Run clippy
clippy:
    nix build .#checks.{{system}}.clippy

# Run unit tests
unit-test:
    nix build .#checks.{{system}}.test

# Run tests
test *args:
    cargo nextest run --no-tests=pass {{args}}

# Auto-fix lint issues
fix:
    cargo clippy --all-targets --fix --allow-dirty --allow-staged

# Clean build artifacts
clean:
    cargo clean
