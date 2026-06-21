#!/usr/bin/env bash
# Assemble build/Red-<version>-<arch>.AppImage from a release build.
#
#   ./scripts/bundle-linux.sh [version]
#
# A first-cut AppImage: it bundles the Red binary, icon, and .desktop entry and
# relies on the host's system libraries (glibc, X11/Wayland, Vulkan, fontconfig)
# — present on any Linux desktop. Hardening to a fully self-contained image
# (bundling libxkbcommon etc. via linuxdeploy) is a follow-up; see
# docs/plans/future/cross-platform.md.
#
# Needs: rsvg-convert (librsvg) for the icon; appimagetool (auto-downloaded to
# build/ if not on PATH). Run on Linux.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:-$(git -C "$ROOT" describe --tags --always | sed 's/^v//')}"
ARCH="$(uname -m)"
BUILD="$ROOT/build"
APPDIR="$BUILD/Red.AppDir"

echo "▸ cargo build --release"
cargo build -p red --release
BIN="$ROOT/target/release/Red"

echo "▸ assembling AppDir (v$VERSION, $ARCH)"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps"
cp "$BIN" "$APPDIR/usr/bin/Red"

# Icon: a scalable SVG plus rasterized PNGs at the standard hicolor sizes.
cp "$ROOT/assets/red.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/red.svg"
for size in 32 64 128 256 512; do
  dir="$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps"
  mkdir -p "$dir"
  rsvg-convert -w "$size" -h "$size" "$ROOT/assets/red.svg" -o "$dir/red.png"
done
# AppImage expects a top-level icon + .desktop next to AppRun.
cp "$APPDIR/usr/share/icons/hicolor/512x512/apps/red.png" "$APPDIR/red.png"
cp "$ROOT/crates/red/resources/red.desktop" "$APPDIR/usr/share/applications/red.desktop"
cp "$ROOT/crates/red/resources/red.desktop" "$APPDIR/red.desktop"

# AppRun: launch the bundled binary. exec so signals reach Red directly.
cat > "$APPDIR/AppRun" <<'EOF'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "${0}")")"
exec "${HERE}/usr/bin/Red" "$@"
EOF
chmod +x "$APPDIR/AppRun" "$APPDIR/usr/bin/Red"

# appimagetool: prefer one on PATH, else fetch the official AppImage into build/.
TOOL="$(command -v appimagetool || true)"
if [[ -z "$TOOL" ]]; then
  TOOL="$BUILD/appimagetool-x86_64.AppImage"
  if [[ ! -x "$TOOL" ]]; then
    echo "▸ downloading appimagetool"
    curl -fsSL -o "$TOOL" \
      "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
    chmod +x "$TOOL"
  fi
fi

OUT="$BUILD/Red-$VERSION-$ARCH.AppImage"
echo "▸ packaging $OUT"
# ARCH guides appimagetool's runtime selection; --no-appstream skips the metainfo
# validation we don't ship yet. Falls back to extract-and-run where FUSE is absent
# (CI containers), so it works without /dev/fuse.
ARCH="$ARCH" "$TOOL" --no-appstream "$APPDIR" "$OUT" \
  || ARCH="$ARCH" "$TOOL" --appimage-extract-and-run --no-appstream "$APPDIR" "$OUT"

# Sidecar checksum: the Linux self-updater verifies the downloaded AppImage against
# this (AppImages aren't OS-notarized). Written with a bare filename so the digest
# is the first token, which the updater parses. Keep it next to the artifact.
( cd "$BUILD" && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256" )
echo "▸ done: $OUT (+ .sha256)"
