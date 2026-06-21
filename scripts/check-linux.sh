#!/usr/bin/env bash
# Build Red for Linux in a Docker container, from a macOS (or any) host.
#
# Red has no macOS lock-in in its own code, but GPUI's Linux backend (x11 +
# wayland) needs a handful of system -dev packages to compile. This script spins
# up an official `rust` image, installs those, and builds `-p red` against a
# throwaway target dir so it never clobbers the host's macOS `target/`.
#
# Registry + target are cached in named volumes so re-runs are fast. Pass an
# alternate cargo subcommand as args, e.g. `scripts/check-linux.sh check`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_CMD=("${@:-build}")

# System deps for GPUI's Linux backend (x11/wayland) + bundled C libs (rusqlite).
# Kept close to Zed's documented Linux build set.
DEPS="pkg-config clang cmake protobuf-compiler \
  libfontconfig-dev libfreetype-dev \
  libwayland-dev wayland-protocols libxkbcommon-dev libxkbcommon-x11-dev \
  libx11-dev libx11-xcb-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev \
  libxcb-xfixes0-dev libxcb-shm0-dev \
  libasound2-dev libssl-dev libvulkan-dev libgbm-dev"

exec docker run --rm \
  -v "${REPO_ROOT}:/red" \
  -v red-cargo-registry:/usr/local/cargo/registry \
  -v red-cargo-git:/usr/local/cargo/git \
  -v red-target-linux:/target-linux \
  -e CARGO_TARGET_DIR=/target-linux \
  -w /red \
  rust:1-bookworm \
  bash -euxc "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ${DEPS} >/dev/null && cargo ${CARGO_CMD[*]} -p red"
