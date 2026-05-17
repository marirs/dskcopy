#!/usr/bin/env bash
# Generate assets/dskcopy.icns from assets/dskcopy-icon.svg using built-in
# macOS tools (qlmanage, sips, iconutil). No external dependencies required.
#
# Usage: bash scripts/make-icns.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SVG="$ROOT_DIR/assets/dskcopy-icon.svg"
OUT_ICNS="$ROOT_DIR/assets/dskcopy.icns"

if [[ ! -f "$SVG" ]]; then
  echo "ERROR: source SVG not found at $SVG" >&2
  exit 1
fi

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

ICONSET_DIR="$WORK_DIR/dskcopy.iconset"
mkdir -p "$ICONSET_DIR"

# Step 1: render SVG -> 1024 PNG. Try rsvg-convert first, then qlmanage.
MASTER_PNG="$WORK_DIR/master.png"
if command -v rsvg-convert >/dev/null 2>&1; then
  echo "Rendering SVG via rsvg-convert..."
  rsvg-convert -w 1024 -h 1024 "$SVG" -o "$MASTER_PNG"
else
  echo "Rendering SVG via qlmanage..."
  qlmanage -t -s 1024 -o "$WORK_DIR" "$SVG" >/dev/null 2>&1 || true
  # qlmanage names the output "<basename>.png"
  CANDIDATE="$WORK_DIR/$(basename "$SVG").png"
  if [[ -f "$CANDIDATE" ]]; then
    mv "$CANDIDATE" "$MASTER_PNG"
  else
    echo "ERROR: qlmanage failed to render $SVG to PNG." >&2
    echo "Install librsvg (e.g. 'brew install librsvg') and re-run." >&2
    exit 1
  fi
fi

# Step 2: produce all sizes the iconset format expects.
gen() {
  local size="$1"
  local name="$2"
  sips -z "$size" "$size" "$MASTER_PNG" --out "$ICONSET_DIR/$name" >/dev/null
}

gen 16   icon_16x16.png
gen 32   icon_16x16@2x.png
gen 32   icon_32x32.png
gen 64   icon_32x32@2x.png
gen 128  icon_128x128.png
gen 256  icon_128x128@2x.png
gen 256  icon_256x256.png
gen 512  icon_256x256@2x.png
gen 512  icon_512x512.png
gen 1024 icon_512x512@2x.png

# Step 3: pack iconset into icns.
iconutil -c icns "$ICONSET_DIR" -o "$OUT_ICNS"
echo "Wrote $OUT_ICNS"

# Step 4: also publish a 256px PNG for the Linux .desktop entry.
OUT_PNG="$ROOT_DIR/assets/icon_256.png"
cp "$ICONSET_DIR/icon_256x256.png" "$OUT_PNG"
echo "Wrote $OUT_PNG"
