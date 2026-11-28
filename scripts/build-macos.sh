#!/bin/bash
# Builds, for Apple Silicon, into dist/: HubBridge.app, HubBridge-macOS.dmg, HubBridge-<version>-macos-arm64.zip (update
# archive) and hubbridge-<version>-macos-arm64.tar.gz (CLI + APK).
# Run: scripts/build-macos.sh [--apk <path>] [--skip-tests]
set -euo pipefail
cd "$(dirname "$0")/.."

apk=""
run_tests=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --apk) apk=${2:?--apk needs a path}; shift 2 ;;
    --skip-tests) run_tests=0; shift ;;
    *) echo "usage: $0 [--apk <path>] [--skip-tests]" >&2; exit 2 ;;
  esac
done

target=aarch64-apple-darwin
version=$(scripts/version.sh)
app=dist/HubBridge.app
cli=dist/cli

if [[ -z "$apk" && -f android/app/build/outputs/apk/release/app-release.apk ]]; then
  apk=android/app/build/outputs/apk/release/app-release.apk
fi
if [[ -n "$apk" && ! -f "$apk" ]]; then
  echo "APK not found: $apk" >&2
  exit 1
fi
if [[ -z "$apk" ]]; then
  echo "warning: no APK given or built, 'Install app' will report that no APK is bundled" >&2
fi

rustup target add "$target" >/dev/null
if [[ $run_tests -eq 1 ]]; then
  cargo test --workspace --quiet
fi

rm -rf "$app" "$cli" dist/HubBridge-macOS.dmg dist/HubBridge-*-macos-arm64.zip dist/hubbridge-*-macos-arm64.tar.gz
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$cli"

# HubBridge (window) and hubbridge (CLI) are the same path on a case-insensitive disk: copy each one right after its build.
cargo build --release --target "$target" -p hubbridge-desktop
cp "target/$target/release/HubBridge" "$app/Contents/MacOS/HubBridge"
cargo build --release --target "$target" -p hubbridge-host --bin hubbridge
cp "target/$target/release/hubbridge" "$cli/hubbridge"
if cmp -s "$app/Contents/MacOS/HubBridge" "$cli/hubbridge"; then
  echo "the window binary and the CLI binary are identical: the build overwrote one with the other" >&2
  exit 1
fi

icon_keys=""
if [[ -f crates/desktop/assets/AppIcon.icns ]]; then
  cp crates/desktop/assets/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"
  icon_keys="<key>CFBundleIconFile</key><string>AppIcon</string>"
else
  echo "warning: crates/desktop/assets/AppIcon.icns is missing, the app gets no icon" >&2
fi

if [[ -n "$apk" ]]; then
  cp "$apk" "$app/Contents/Resources/HubBridge.apk"
  cp "$apk" "$cli/HubBridge.apk"
fi
cp LICENSE "$cli/LICENSE"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>HubBridge</string>
  <key>CFBundleDisplayName</key><string>HubBridge</string>
  <key>CFBundleIdentifier</key><string>com.demoz.hubbridge</string>
  <key>CFBundleExecutable</key><string>HubBridge</string>
  $icon_keys
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleLocalizations</key>
  <array><string>en</string><string>ru</string></array>
  <key>NSLocalNetworkUsageDescription</key><string>HubBridge finds your Android phone on the local network and connects to it to use the USB devices on its hub.</string>
  <key>NSBonjourServices</key>
  <array><string>_hubbridge._tcp</string></array>
</dict>
</plist>
PLIST
printf 'APPL????' > "$app/Contents/PkgInfo"
plutil -lint "$app/Contents/Info.plist"

# ad hoc signature: without it Apple Silicon won't start a downloaded binary even after removing quarantine
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"
codesign --force --sign - "$cli/hubbridge"

# update archive: ditto keeps the signature and attributes of the .app
ditto -c -k --keepParent "$app" "dist/HubBridge-$version-macos-arm64.zip"

staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
# GitHub's macOS runners fail hdiutil now and then with "Resource busy".
for attempt in 1 2 3; do
  if hdiutil create -volname "HubBridge" -srcfolder "$staging" -ov -format UDZO dist/HubBridge-macOS.dmg; then
    break
  fi
  if [[ $attempt -eq 3 ]]; then
    echo "hdiutil create failed $attempt times" >&2
    exit 1
  fi
  echo "warning: hdiutil create failed (attempt $attempt of 3), retrying" >&2
  sleep 5
done

files=(hubbridge LICENSE)
[[ -f "$cli/HubBridge.apk" ]] && files=(hubbridge HubBridge.apk LICENSE)
tar -czf "dist/hubbridge-$version-macos-arm64.tar.gz" -C "$cli" "${files[@]}"

echo "Done: $app, dist/HubBridge-macOS.dmg, dist/HubBridge-$version-macos-arm64.zip, dist/hubbridge-$version-macos-arm64.tar.gz"
