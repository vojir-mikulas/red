#!/usr/bin/env bash
# Assemble build/Red.app from a release build. Universal (arm64+x86_64) by
# default; pass ARCH=native to build only the host arch (faster for local test).
#
#   ./scripts/bundle-mac.sh [version]
#
# Then sign + notarize with ./scripts/sign-mac.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:-$(git -C "$ROOT" describe --tags --always | sed 's/^v//')}"
APP="$ROOT/build/Red.app"
ARCH="${ARCH:-universal}"

if [[ "$ARCH" == "native" ]]; then
  echo "▸ cargo build --release (native)"
  cargo build -p red --release
  BIN="$ROOT/target/release/Red"
else
  echo "▸ cargo build --release (universal: aarch64 + x86_64)"
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null 2>&1 || true
  cargo build -p red --release --target aarch64-apple-darwin
  cargo build -p red --release --target x86_64-apple-darwin
  mkdir -p "$ROOT/build"
  lipo -create -output "$ROOT/build/Red" \
    "$ROOT/target/aarch64-apple-darwin/release/Red" \
    "$ROOT/target/x86_64-apple-darwin/release/Red"
  BIN="$ROOT/build/Red"
fi

echo "▸ assembling $APP (v$VERSION)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/Red"
sed "s/__VERSION__/$VERSION/g" "$ROOT/crates/red/resources/Info.plist" \
  > "$APP/Contents/Info.plist"
if [[ -f "$ROOT/build/Red.icns" ]]; then
  cp "$ROOT/build/Red.icns" "$APP/Contents/Resources/Red.icns"
else
  echo "  (no build/Red.icns — app builds without an icon; run scripts/make-icns.sh)"
fi
echo "▸ done: $APP"
