# RED task runner. Install `just`: `brew install just`.
# Run `just` with no args to list recipes.

# Default: list available recipes.
default:
    @just --list

# Run the app.
run:
    cargo run -p red

# Run the Flint component gallery ("storybook"). Lives in the sibling Flint repo.
gallery:
    cargo run --manifest-path ../flint/Cargo.toml --example gallery

# Build the whole workspace.
build:
    cargo build --workspace

# Run all tests.
test:
    cargo test --workspace

# Lint with clippy, warnings as errors.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format all code.
fmt:
    cargo fmt --all

# Check formatting without writing.
fmt-check:
    cargo fmt --all --check

# The pre-push gate: format, lint, test.
check: fmt lint test
