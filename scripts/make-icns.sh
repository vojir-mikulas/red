#!/usr/bin/env bash
# Generate an .icns from a square PNG master (≥1024×1024 recommended).
#
#   ./scripts/make-icns.sh assets/icon-1024.png build/Red.icns
#
# Get a PNG master from the brand SVG with librsvg (brew install librsvg):
#   rsvg-convert -w 1024 -h 1024 assets/red.svg -o assets/icon-1024.png
set -euo pipefail
SRC="${1:?usage: make-icns.sh <icon-1024.png> [out.icns]}"
OUT="${2:-build/Red.icns}"
ICONSET="$(mktemp -d)/Red.iconset"; mkdir -p "$ICONSET"

for size in 16 32 128 256 512; do
  sips -z "$size" "$size"               "$SRC" --out "$ICONSET/icon_${size}x${size}.png"    >/dev/null
  sips -z $((size*2)) $((size*2))       "$SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
mkdir -p "$(dirname "$OUT")"
iconutil -c icns "$ICONSET" -o "$OUT"
echo "wrote $OUT"
