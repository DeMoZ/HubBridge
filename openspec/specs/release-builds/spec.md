# release-builds Specification

## Purpose
Defines what a HubBridge version tag publishes on GitHub — the macOS app, the Android app and
the CLI, with fixed names, versions and signatures — and the checks that stop a broken release,
so people and the in-app updater can rely on it.

## Requirements

### Requirement: Release from a version tag
Pushing a tag `vX.Y.Z` SHALL build and publish a GitHub release named "HubBridge vX.Y.Z" for that tag, marked as the latest release, without manual steps.

#### Scenario: Tag pushed
- **WHEN** the tag `v0.1.0` is pushed and the workspace version is 0.1.0
- **THEN** a release "HubBridge v0.1.0" appears with all release files, and `https://api.github.com/repos/DeMoZ/HubBridge/releases/latest` returns it

### Requirement: Tag must match the version
The release SHALL be published only when the tag equals `v` + the workspace version in `Cargo.toml`; on a mismatch the build SHALL stop with an error naming both values and SHALL publish nothing. The tagged commit SHALL also be on the main branch; otherwise the build SHALL stop with an error and SHALL publish nothing.

#### Scenario: Mismatch
- **WHEN** the tag `v0.2.0` is pushed while `Cargo.toml` says 0.1.0
- **THEN** the workflow fails with "Tag v0.2.0 doesn't match Cargo.toml (0.1.0)" and no release is created

#### Scenario: Tag outside main
- **WHEN** a tag `v0.1.0` is pushed on a commit that is not an ancestor of `origin/main`
- **THEN** the workflow fails early with an error saying the tagged commit is not on main, and no release is created

### Requirement: Release files and names
Each release SHALL contain exactly: `HubBridge-macOS-vX.Y.Z.dmg`, `HubBridge-X.Y.Z-macos-arm64.zip`, `HubBridge-X.Y.Z.apk` and `hubbridge-X.Y.Z-macos-arm64.tar.gz`. No Windows and no Linux files SHALL be published.

#### Scenario: Assets of v0.1.0
- **WHEN** the v0.1.0 release is published
- **THEN** its assets are exactly the four files above with version 0.1.0, each with a SHA-256 digest shown by GitHub

### Requirement: macOS app bundle
The `.dmg` and the `-macos-arm64.zip` SHALL each contain `HubBridge.app` for Apple Silicon: bundle id `com.demoz.hubbridge`, version X.Y.Z, minimum macOS 11, the HubBridge icon, `NSLocalNetworkUsageDescription`, `NSBonjourServices` with `_hubbridge._tcp`, and a valid (ad-hoc) code signature. The `.dmg` SHALL also hold a link to Applications.

#### Scenario: Bundle metadata
- **WHEN** the zip is unpacked and `Contents/Info.plist` is read
- **THEN** it shows `CFBundleIdentifier` `com.demoz.hubbridge`, `CFBundleShortVersionString` X.Y.Z, `NSBonjourServices` containing `_hubbridge._tcp` and a non-empty `NSLocalNetworkUsageDescription`, and `codesign --verify --deep --strict` succeeds on the app

#### Scenario: Local network on first start
- **WHEN** the app from the `.dmg` is started for the first time on a Mac
- **THEN** macOS asks whether HubBridge may find devices on the local network, with the app's description text, and after Allow the window lists phones found over mDNS

### Requirement: Phone app inside the desktop app
`HubBridge.app` SHALL contain the release APK of the same version at `Contents/Resources/HubBridge.apk`, so "Install app" in the window and `hubbridge install-apk` work without a separate download.

#### Scenario: Install from the packaged app
- **WHEN** a phone with USB debugging is plugged in and the user presses "Install app" in the packaged window
- **THEN** the bundled APK of the same version is installed and opened on the phone

### Requirement: Android release APK
`HubBridge-X.Y.Z.apk` SHALL be a release build for arm64-v8a, armeabi-v7a and x86_64 with `versionName` X.Y.Z and `versionCode` X×10000 + Y×100 + Z, signed with the project's stable release key. A tag build without that key SHALL fail instead of publishing an APK signed with another key.

#### Scenario: Version of the APK
- **WHEN** the v0.1.0 APK is inspected with `aapt2 dump badging`
- **THEN** it shows `versionName='0.1.0'` and `versionCode='100'`, and `apksigner verify` succeeds

#### Scenario: Same key across releases
- **WHEN** v0.1.0 is installed on a phone and the v0.1.1 APK is installed over it
- **THEN** the install succeeds as an update and the phone keeps its paired computers

#### Scenario: Key missing
- **WHEN** a tag is pushed and the signing secrets are not set
- **THEN** the workflow fails with a message naming the missing secrets and no release is created

### Requirement: CLI archive
`hubbridge-X.Y.Z-macos-arm64.tar.gz` SHALL contain the `hubbridge` command-line tool for Apple Silicon and `HubBridge.apk` next to it, so `hubbridge install-apk` finds the APK without options.

#### Scenario: CLI from the archive
- **WHEN** the archive is unpacked and `./hubbridge install-apk` runs with a phone on USB
- **THEN** it reports the APK source as bundled and installs version X.Y.Z

### Requirement: Release notes
The release body SHALL be the file `docs/releases/vX.Y.Z.md` when it exists (generated notes otherwise), with English and Russian "What's new" parts and "Install" / "Установка" sections last.

#### Scenario: Notes file present
- **WHEN** `docs/releases/v0.1.0.md` exists and v0.1.0 is released
- **THEN** the release body is that file's content

### Requirement: Dry run without publishing
A manual run of the release workflow SHALL build every release file and keep them as workflow artifacts without creating a release or a tag.

#### Scenario: Manual run on a branch
- **WHEN** the release workflow is started by hand on a branch (once the workflow is on the default branch)
- **THEN** the run finishes with the four files as artifacts and the repository has no new release

#### Scenario: Pull request touching the release files
- **WHEN** a pull request changes the release workflow, the build or version scripts, the Gradle build file or the release notes
- **THEN** the release workflow runs as a dry run: the four files are built as artifacts, no release or tag is created

### Requirement: Local build of the macOS files
The macOS files SHALL be buildable on a developer Mac with one script that produces the same `.app`, `.dmg`, zip and CLI archive as CI, with an option to bundle a given APK.

#### Scenario: Local build
- **WHEN** Roman runs the macOS build script on his Mac with an APK path
- **THEN** `dist/` holds `HubBridge.app` (with the APK inside), the `.dmg`, the update zip and the CLI archive for the current version
