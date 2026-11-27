#!/bin/bash
# Redraws the app icon: crates/desktop/assets/icon.png (1024) and crates/desktop/assets/AppIcon.icns (macOS).
# Needs macOS (swift, sips, iconutil). Both outputs are committed; CI does not run this.
set -euo pipefail
cd "$(dirname "$0")/.."
assets=crates/desktop/assets
mkdir -p "$assets"
work=$(mktemp -d)
swift scripts/make-icon.swift "$assets/icon.png"

mkdir "$work/AppIcon.iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s "$assets/icon.png" --out "$work/AppIcon.iconset/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$assets/icon.png" --out "$work/AppIcon.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$work/AppIcon.iconset" -o "$assets/AppIcon.icns"
rm -rf "$work"
echo "Done: $assets/icon.png, $assets/AppIcon.icns"
