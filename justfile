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

# Cut a release branch, bump Cargo.toml + Cargo.lock, commit, and push.
# Usage: just bump 0.2.0  (branches from current HEAD)
bump VERSION:
    git checkout -b release/v{{VERSION}}
    sed -i 's/^version = ".*"/version = "{{VERSION}}"/' Cargo.toml
    cargo update -p mdk-recovery
    git add Cargo.toml Cargo.lock
    git commit -m "Release v{{VERSION}}"
    git push -u origin release/v{{VERSION}}
