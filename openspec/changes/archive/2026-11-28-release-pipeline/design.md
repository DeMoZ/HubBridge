# Design

## Context

- Workspace version `0.1.0` in `[workspace.package]` of `Cargo.toml`; every crate uses
  `version.workspace = true`. Android has its own `versionName = "0.1.0"`, `versionCode = 1` in
  `android/app/build.gradle.kts`; its `cargoNdk` task builds the `.so` files with `--release`
  when the Gradle task name contains "Release".
- Binaries: the window `HubBridge` (crate `hubbridge-desktop`) and the CLI `hubbridge` (crate
  `hubbridge-host`). **On a case-insensitive APFS volume (this Mac and GitHub's macOS runners)
  `target/release/HubBridge` and `target/release/hubbridge` are the same file** (noted in the
  archived step 2b tasks): building one overwrites the other.
- `hubbridge_host::install::apk_candidates` already looks for `HubBridge.apk` next to the
  executable and in `../Resources` (the macOS bundle), before the dev checkout's Gradle output.
- CI (`ci.yml`): Rust on macOS + Windows (fmt, clippy, test), APK debug build on Ubuntu with
  NDK 27.2.12479018 and cargo-ndk. No release workflow, no repository secrets.
- Reference: UsbMemoryChecker `.github/workflows/build.yml` + `scripts/build-macos.sh` (the
  closest sibling, also Rust + tao + wry): version check against the tag, `.app` assembled by
  hand with a heredoc `Info.plist`, ad-hoc `codesign`, `ditto -c -k --keepParent` zip for the
  updater, `hdiutil` UDZO `.dmg` with an Applications link, release body from
  `docs/releases/<tag>.md` or `--generate-notes`, `gh release create` on Ubuntu. UniversalKM
  `release.yml` is the same shape (Sparkle-specific parts do not apply to a Rust app).

## Goals / Non-Goals

**Goals:**
- One command (`git tag vX.Y.Z && git push origin vX.Y.Z`) publishes a complete release
  whose asset names the updater can rely on (spec "Release files and names").
- One version number to bump (`Cargo.toml`), checked against the tag.
- The phone app can be updated in place from release to release (stable key).
- The same macOS script runs locally and in CI.

**Non-Goals:**
- No Windows or Linux release files; no Intel/universal macOS build (proposal, open
  questions 3–4).
- No Developer ID signing, notarization or stapling (open question 2); no Sparkle.
- No Play Store / AAB.
- No changes to `ci.yml` behavior; PR checks stay as they are.
- The adb "disclaimed spawn" fix from ROADMAP (adb server keeps network access after the app
  quits) is a separate change; it is listed as a known limitation.

## Decisions

### 1. One version source: `Cargo.toml`, read by Gradle and the scripts

`scripts/version.sh` prints the `[workspace.package]` version (awk over `Cargo.toml`; no
`cargo metadata`, which would print the first package's version and needs a toolchain).
`build.gradle.kts` reads the same value with a regex over `../Cargo.toml` and sets
`versionName = it`, `versionCode = major * 10000 + minor * 100 + patch` (0.1.0 → 100, larger
than today's 1, so an installed dev build updates without a downgrade error). The workflow
compares `v$(scripts/version.sh)` with `$GITHUB_REF_NAME` in every job that runs on a tag.

Alternative: keep two numbers and check they match (UsbMemoryChecker has one crate, so it
never needed this) — rejected, a second place to forget.

### 2. Android release signing from environment variables

`build.gradle.kts` gets a `release` signing config filled from `HUBBRIDGE_KEYSTORE_FILE`,
`HUBBRIDGE_KEYSTORE_PASSWORD`, `HUBBRIDGE_KEY_ALIAS`, `HUBBRIDGE_KEY_PASSWORD`. When they are
not set, the `release` build type falls back to the debug signing config and Gradle logs a
warning (local `assembleRelease` keeps working). In the workflow the `android` job decodes
`secrets.ANDROID_KEYSTORE_BASE64` into `$RUNNER_TEMP/release.keystore` with
`printf '%s' "$ANDROID_KEYSTORE_BASE64" | base64 --decode > …` (never echoed) and exports the
four variables; on a tag, a missing secret stops the job with
`::error::ANDROID_KEYSTORE_BASE64 … not set`; on a manual run it only warns and builds with
debug signing (the artifact is a dry run). After the build: `apksigner verify` must succeed.
Passwords are passed to Gradle through the environment only, never as `-P` arguments (they
would show in the process list and logs).

Roman creates the key once (command in tasks.md, group 2; it prompts for the passwords, so
nothing secret is on a command line) and sets the secrets with `gh secret set NAME` reading
from stdin. The keystore file and its passwords must be backed up by Roman: losing them means
every phone has to uninstall the app (and forget its pairings) to get the next release.

Alternatives: commit a debug keystore (simple, but anyone could sign an "update" for
HubBridge); sign locally and upload by hand (UniversalKM does this for its prebuilt APKs; it
breaks "one tag publishes everything").

### 3. macOS bundle script (`scripts/build-macos.sh`)

Modelled line by line on UsbMemoryChecker's script, with these differences:

```
scripts/build-macos.sh [--apk <path>] [--skip-tests]
```

1. `target=aarch64-apple-darwin`; `rustup target add`; unless `--skip-tests`, `cargo test --workspace --quiet`.
2. `cargo build --release --target $target -p hubbridge-desktop`, then **immediately** copy
   `target/$target/release/HubBridge` into the bundle; then
   `cargo build --release --target $target -p hubbridge-host --bin hubbridge` and copy
   `target/$target/release/hubbridge` into `dist/cli/` (see Context: the two names
   collide). After both copies, check they differ (`cmp -s` must fail) so a collision
   cannot slip through.
3. Bundle: `dist/HubBridge.app/Contents/{MacOS/HubBridge, Resources/AppIcon.icns, Info.plist, PkgInfo}`.
   Icon from `crates/desktop/assets/AppIcon.icns`; if missing, warn and leave
   `CFBundleIconFile` out.
4. APK: `--apk <path>` (CI passes the release APK), else
   `android/app/build/outputs/apk/release/app-release.apk` if present, else none with a warning
   ("Install app" then reports no APK). Copied to `Contents/Resources/HubBridge.apk` and to
   `dist/cli/HubBridge.apk`.
5. `Info.plist` (heredoc, version from `scripts/version.sh`): `CFBundleName`/`DisplayName`
   HubBridge, `CFBundleIdentifier` `com.demoz.hubbridge`, `CFBundleExecutable` HubBridge,
   `CFBundleIconFile` AppIcon, `CFBundlePackageType` APPL, `CFBundleShortVersionString` and
   `CFBundleVersion` = version, `LSMinimumSystemVersion` 11.0, `NSHighResolutionCapable`,
   `LSApplicationCategoryType` `public.app-category.utilities`, `CFBundleDevelopmentRegion` en,
   `CFBundleLocalizations` [en, ru], `NSLocalNetworkUsageDescription` "HubBridge finds your
   Android phone on the local network and connects to it to use the USB devices on its hub.",
   `NSBonjourServices` [`_hubbridge._tcp`]. Then `plutil -lint`.
6. `codesign --force --deep --sign - dist/HubBridge.app` (ad-hoc; Apple Silicon refuses to run
   an unsigned downloaded binary), `codesign --verify --deep --strict`. The CLI binary is
   already ad-hoc signed by the linker on arm64; `codesign --force --sign -` it anyway for
   symmetry.
7. `ditto -c -k --keepParent dist/HubBridge.app dist/HubBridge-$version-macos-arm64.zip`
   (keeps the signature and attributes; the in-app updater unpacks with `ditto -x -k`).
8. `.dmg`: staging dir with the app + `Applications` symlink,
   `hdiutil create -volname HubBridge -srcfolder … -format UDZO dist/HubBridge-macOS.dmg`
   (renamed to `-vX.Y.Z` by the release job, as in UsbMemoryChecker).
9. CLI archive: `tar -czf dist/hubbridge-$version-macos-arm64.tar.gz -C dist/cli hubbridge HubBridge.apk`
   (+ `LICENSE`).

Bundle id equals the Android application id (`com.demoz.hubbridge`); they live in different
namespaces, so there is no clash.

### 4. Release workflow (`.github/workflows/release.yml`)

```
on: push: tags ['v*']; workflow_dispatch; pull_request with paths [.github/workflows/release.yml,
    scripts/build-macos.sh, scripts/version.sh, android/app/build.gradle.kts, docs/releases/**]
    (a dry run on PRs that touch the release files: GitHub only dispatches workflows that are
    already on the default branch, so a new release.yml cannot be started by hand on a branch;
    a pull_request run behaves like a manual run: version check, keystore and release job skipped
    or warn-only as on workflow_dispatch)
permissions: contents: write
jobs:
  android  (ubuntu-latest): checkout, Java 21, Rust + 3 Android targets, rust-cache,
           NDK 27.2.12479018 + cargo-ndk (same steps as ci.yml), version check (tag only),
           keystore from secrets (decision 2), ./gradlew assembleRelease --no-daemon,
           apksigner verify, cp → dist/HubBridge-$version.apk, upload-artifact "android"
  macos    (macos-latest, needs android): checkout, Rust stable + aarch64-apple-darwin,
           rust-cache, version check (tag only), download "android",
           scripts/build-macos.sh --apk dist/HubBridge-$version.apk,
           upload-artifact "macos" (dmg, zip, tar.gz)
  release  (ubuntu-latest, needs [android, macos], if: tag): download all, rename the dmg,
           gh release create "$GITHUB_REF_NAME" --title "HubBridge $GITHUB_REF_NAME"
           --notes-file docs/releases/$tag.md (or --generate-notes) <4 files>
```

- Action versions as in UsbMemoryChecker's workflow (`actions/checkout@v7`,
  `actions/upload-artifact@v7`, `actions/download-artifact@v8`), `dtolnay/rust-toolchain@stable`,
  `Swatinem/rust-cache@v2`, `actions/setup-java@v4` as in ci.yml.
- The macOS job runs `cargo test --workspace` through the script (a release must not be cut from
  a red tree even if CI was skipped); fmt and clippy stay in ci.yml.
- `gh release create` marks the newest non-prerelease as latest by default, which is what
  `releases/latest` (the updater's endpoint) returns.
- Ubuntu runs only the APK build and the publish step, as ci.yml already does for the APK; no
  HubBridge binary for Linux is built.

### 5. Release notes file

`docs/releases/v0.1.0.md`: `## What's new` (English), `## Что нового` (Russian), then
`## Install` and `## Установка` last — the update window of `desktop-updates-donate` cuts the
text at the first of those two headings, as UsbMemoryChecker does. Install text: macOS (open
the `.dmg`, drag to Applications; first start: macOS says it cannot verify the app → System
Settings → Privacy & Security → "Open Anyway", or `xattr -dr com.apple.quarantine
/Applications/HubBridge.app`; allow Local Network), Android (Install app in the window, or
`adb install HubBridge-X.Y.Z.apk`; a dev build signed with another key must be uninstalled
first, which forgets its paired computers).

### 6. Signing honesty

Mirrors UsbMemoryChecker: ad-hoc signed, not notarized. Documented in README (both
languages), LIMITATIONS and the release notes. What this means for users:
- first start needs the "Open Anyway" step once per download;
- macOS may ask for Local Network access again after an update, because an ad-hoc signed app
  has no stable signing identity (Apple's local network privacy keys apps by their executable
  identity); checked on the first real update and documented either way;
- the in-app updater is not affected by Gatekeeper: files downloaded with `curl` carry no
  quarantine flag, and the updater's script removes it anyway.

## Risks / Trade-offs

- [The two binaries overwrite each other on case-insensitive disks] → build-and-copy in
  sequence plus the `cmp` check (decision 3, step 2).
- [Lost or leaked Android key] → Roman keeps an offline backup; the key never leaves secrets
  and his machine; the workflow never prints it (no `set -x` in that step).
- [Users with a debug-signed phone app (Roman's own phone) cannot update in place] → release
  notes and the "Install app" error hint say to uninstall once; the desktop window has no
  "replace" button in this change (CLI has `--replace-incompatible`).
- [Gatekeeper wording and steps change between macOS versions] → the notes name the System
  Settings path and the `xattr` fallback; checked on this Mac (macOS 27) during the first
  release.
- [The adb server spawned by the packaged app may lose network access after the app quits
  (UniversalKM `fix-adb-server-local-network`)] → listed in LIMITATIONS; ROADMAP item stays
  open.
- [`macos-latest` image changes Xcode / SDK] → the script uses only `codesign`, `ditto`,
  `hdiutil` and `plutil`, which have been stable for years; the icon is prebuilt and committed.

## Migration Plan

1. Merge this change (with the other two) to main — Roman's step.
2. Roman creates the keystore and sets the four secrets (tasks group 2).
3. A manual run of the release workflow on main succeeds (dry run).
4. The maintainer tags `v0.1.0` on main and pushes the tag; the workflow publishes.
5. Rollback: delete the release and the tag (`gh release delete v0.1.0 --cleanup-tag`); nothing
   else depends on it yet.
