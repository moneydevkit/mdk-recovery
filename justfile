default:
    @just --list

# Run all checks (fmt, clippy, unit tests)
check: fmt-check clippy test

# Format code
fmt:
    cargo fmt
    nixfmt flake.nix

# Check formatting
fmt-check:
    cargo fmt --check
    nixfmt --check flake.nix

# Run clippy
clippy:
    cargo clippy --all-targets -- --deny warnings

# Run tests
test *args:
    cargo nextest run --no-tests=pass {{args}}

# Auto-fix lint issues
fix:
    cargo clippy --all-targets --fix --allow-dirty --allow-staged

# Clean build artifacts
clean:
    cargo clean
