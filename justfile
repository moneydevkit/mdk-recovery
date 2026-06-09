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

# Cut a release: branch, bump Cargo.toml + Cargo.lock, push, and open the PR.
# Merging that PR triggers the Release workflow (tag + build + publish).
# Usage: just release 0.2.0  (branches from current HEAD)
release VERSION:
    git checkout -b release/v{{VERSION}}
    sed -i 's/^version = ".*"/version = "{{VERSION}}"/' Cargo.toml
    cargo update -p mdk-recovery
    git add Cargo.toml Cargo.lock
    git commit -m "Release v{{VERSION}}"
    git push -u origin release/v{{VERSION}}
    gh pr create --base master --head release/v{{VERSION}} \
        --title "Release v{{VERSION}}" \
        --body "$(printf 'Merging this PR tags v{{VERSION}} and publishes.\n\n## Changes\n\n'; git log --reverse --merges --pretty=format:'- %b (%s)' "$(git describe --tags --abbrev=0 --match 'v*' HEAD^ 2>/dev/null || git rev-list --max-parents=0 HEAD | tail -1)..HEAD^" | sed -E 's/ \(Merge pull request (#[0-9]+) from .*\)$/ (\1)/')"
