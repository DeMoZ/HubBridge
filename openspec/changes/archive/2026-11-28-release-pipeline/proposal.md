# Proposal

## Open questions

Defaults are taken from the reference projects so the plan can go ahead; each one can be
changed before `/opsx:apply`.

1. **Android release signing key (needs Roman).** Every release APK must be signed with the
   same key, or the phone app cannot be updated in place (adb refuses
   `INSTALL_FAILED_UPDATE_INCOMPATIBLE`, and uninstalling forgets the paired computers). Default:
   a new release keystore that Roman creates and keeps a backup of, given to CI as four
   repository secrets (`ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`,
   `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`); a tag build fails without them. UniversalKM
   signs its APKs with a local key the same way (`~/.config/universalkm/motion-release.keystore`),
   so reusing that key is possible too, but a separate key per app is the default.
2. **macOS signing.** Default: ad-hoc signature, no notarization, exactly like
   UsbMemoryChecker (`codesign --force --deep --sign -`). None of the reference projects has a
   Developer ID; UniversalKM uses a self-signed certificate only to keep Accessibility grants,
   which HubBridge does not need. Users see the Gatekeeper "could not verify" dialog once and
   open the app through System Settings → Privacy & Security → Open Anyway (documented). A
   Developer ID ($99/year) + notarization would remove that; say so if wanted.
3. **Mac architectures.** Default: Apple Silicon only (`aarch64-apple-darwin`), like
   UsbMemoryChecker. A universal binary would double the build; say so if Intel Macs matter.
4. **CLI archive.** Default: yes, `hubbridge-<version>-macos-arm64.tar.gz` with the `hubbridge`
   CLI and the APK. Windows: no release builds of anything (code and CI checks only).
5. **First version.** Default: `v0.1.0` (the workspace version is already 0.1.0 and no tag
   exists). The Android `versionCode` becomes 100 (from 1), derived from the version.

## Why

HubBridge has never been released: no tag, no release workflow, no packaged app. The window
runs only through `cargo run`, the APK only as a CI debug artifact signed with a throwaway key.
The in-app update check (`desktop-updates-donate`) needs GitHub releases with fixed asset names
to find, and ROADMAP lists `.app` packaging (with the macOS local-network keys) and shipping
the APK next to the desktop app as open items.

## What Changes

- **Tag-driven release workflow** (`.github/workflows/release.yml`): a `v*` tag builds and
  publishes a GitHub release; a manual run builds the same files without publishing. The tag
  must equal the workspace version, otherwise nothing is published.
- **macOS app**: `HubBridge.app` (Apple Silicon, ad-hoc signed) with an `Info.plist` holding
  `NSLocalNetworkUsageDescription` and `NSBonjourServices = _hubbridge._tcp`, the app icon,
  and the phone app bundled as `Contents/Resources/HubBridge.apk` (the existing "Install app"
  button and `hubbridge install-apk` already look there). Published as a `.dmg` for people and
  a `-macos-arm64.zip` for the in-app updater, both built by one script
  (`scripts/build-macos.sh`) that also runs locally.
- **Android APK** as a release asset `HubBridge-<version>.apk`: a release build signed with a
  stable key from repository secrets; `versionName` / `versionCode` come from the workspace
  version, so there is one version number to bump.
- **CLI archive** `hubbridge-<version>-macos-arm64.tar.gz` (`hubbridge` + `HubBridge.apk`).
- **Release notes** in `docs/releases/v<version>.md` (English + Russian, an "Install" section
  the update window hides), used as the release body.
- **No Windows release builds and no Linux desktop builds.** Windows stays compiled, linted
  and tested in CI (`ci.yml` unchanged); the Android build and the publish step run on the
  hosted Ubuntu runner as they already do, which is not a Linux build of HubBridge.
- Docs: install instructions with the Gatekeeper step, how to cut a release, the required
  secrets.

## Capabilities

### New Capabilities
- `release-builds`: what a version tag publishes (files, names, contents, versions,
  signatures) and the rules that stop a bad release; the contract the in-app updater relies on.

### Modified Capabilities
<!-- none -->

## Impact

- New: `.github/workflows/release.yml`, `scripts/build-macos.sh`, `scripts/version.sh`,
  `docs/releases/v0.1.0.md`.
- Changed: `android/app/build.gradle.kts` (version from `Cargo.toml`, release signing from
  environment variables), `README.md`, `README.ru.md`, new `docs/RELEASING.md` (release steps),
  `docs/ROADMAP.md`, `docs/ARCHITECTURE.md` (layout), `docs/LIMITATIONS.md` (unsigned app,
  Local Network prompt).
- Repository secrets (Roman): the four `ANDROID_*` secrets above. No Apple secrets.
- Depends on `desktop-redesign-2035` for `crates/desktop/assets/AppIcon.icns` (the script
  falls back to no icon if it is missing, with a warning).
- No code or protocol changes in the Rust crates.
