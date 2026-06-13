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

# --- versioning / release ---

# Bump the workspace version (every crate inherits it via `version.workspace`).
# LEVEL is `patch` | `minor` | `major`, or an explicit `X.Y.Z`. Rewrites
# Cargo.toml + refreshes Cargo.lock but does NOT commit — review the diff, move
# the CHANGELOG's _Unreleased_ entry under the new heading, commit, then `just
# tag`. Examples: `just bump minor` · `just bump 0.2.0`.
bump level="patch":
    #!/usr/bin/env bash
    set -euo pipefail
    current=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
    IFS=. read -r major minor patch <<< "$current"
    case "{{level}}" in
      major) new="$((major + 1)).0.0" ;;
      minor) new="${major}.$((minor + 1)).0" ;;
      patch) new="${major}.${minor}.$((patch + 1))" ;;
      [0-9]*.[0-9]*.[0-9]*) new="{{level}}" ;;
      *) echo "level must be patch | minor | major or X.Y.Z (got '{{level}}')" >&2; exit 1 ;;
    esac
    # Only the line-anchored `version = ` in [workspace.package] matches; the
    # inline `version = "1"` dependency pins are not at column 0.
    tmp=$(mktemp)
    sed -E "s/^version = \"[^\"]+\"/version = \"$new\"/" Cargo.toml > "$tmp"
    mv "$tmp" Cargo.toml
    cargo update --workspace --quiet   # sync the workspace crates' versions in Cargo.lock
    echo "Bumped $current -> $new"
    echo "Next: update CHANGELOG (move _Unreleased_ -> $new), commit, then 'just tag'."

# Tag the current Cargo.toml version as vX.Y.Z and push it (fires the release
# CI). Refuses a dirty tree (commit the bump + CHANGELOG first) or an existing
# tag (don't re-cut a shipped version). Run after `just bump`.
tag:
    #!/usr/bin/env bash
    set -euo pipefail
    version=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -n "$(git status --porcelain)" ]; then
      echo "working tree is dirty — commit the version bump + CHANGELOG first" >&2
      exit 1
    fi
    if git rev-parse "v$version" >/dev/null 2>&1; then
      echo "tag v$version already exists — bump the version first" >&2
      exit 1
    fi
    git tag "v$version"
    git push origin "v$version"
    echo "Tagged and pushed v$version — release CI is now building."
