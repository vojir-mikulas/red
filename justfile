# RED task runner. Install `just`: `brew install just`.
# Run `just` with no args to list recipes.

# Default: list available recipes.
default:
    @just --list

# Run the app.
run:
    cargo run -p red

# Release build only — debug timings are several× slower and lie, so only
# optimized numbers are honest. Installs the counting allocator and enables the
# on-screen perf HUD (toggle with ⌥⌘P).
# Run with dev perf instrumentation (optimized + counting allocator + HUD).
run-stats:
    cargo run -p red --release --features dev-stats

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

# --- macOS release (see docs/release-macos.md) ---

# Generate the app icon (needs librsvg: `brew install librsvg`).
icon:
    mkdir -p build
    rsvg-convert -w 1024 -h 1024 assets/red.svg -o build/icon-1024.png
    ./scripts/make-icns.sh build/icon-1024.png build/Red.icns

# Assemble build/Red.app. Native arch (fast); use `bundle-universal` to ship.
bundle:
    ARCH=native ./scripts/bundle-mac.sh

# Assemble a universal (arm64+x86_64) build/Red.app for distribution.
bundle-universal:
    ARCH=universal ./scripts/bundle-mac.sh

# Sign the bundle with the Developer ID identity (no notarization).
sign identity="Developer ID Application: Mikulas Vojir (ZGT84Z73N9)":
    SIGN_IDENTITY="{{identity}}" SKIP_NOTARIZE=1 ./scripts/sign-mac.sh
