# Tasks

Spec: `specs/release-builds/spec.md`. How: `design.md` (decision numbers below). Reference
files (read only, never modify): `/Users/roman/Git/UsbMemoryChecker/scripts/build-macos.sh`,
`/Users/roman/Git/UsbMemoryChecker/.github/workflows/build.yml`,
`/Users/roman/Git/UsbMemoryChecker/docs/releases/v0.1.2.md`. Never print a secret: keystore
contents and passwords are passed through files or the environment only; checks print names and
`set`/`empty`. No Windows or Linux release files. Build on this Mac with
`ANDROID_HOME=/opt/homebrew/share/android-commandlinetools` and
`JAVA_HOME=$(/usr/libexec/java_home -v 25)` for Gradle. Depends on `desktop-redesign-2035`
(app icon); if that is not merged yet, the script's no-icon fallback is what you see.

## 1. One version number

- [x] 1.1 Add `scripts/version.sh` (prints the `[workspace.package]` version of `Cargo.toml`, exits non-zero if not found; design decision 1); verify `scripts/version.sh` prints `0.1.0` and prints an error + exit 1 when run against a scratch copy of `Cargo.toml` without the `[workspace.package]` version (pass the file as an optional argument)
  - Result (2026-11-28): prints `0.1.0`; on a scratch copy without the version it prints `version.sh: no version in [workspace.package] of …` and exits 1.
- [x] 1.2 `android/app/build.gradle.kts`: read the version from `../Cargo.toml`, set `versionName` and `versionCode = major*10000 + minor*100 + patch` (decision 1), fail the Gradle configuration with a clear message if it cannot be parsed; verify `./gradlew assembleDebug` succeeds and `$ANDROID_HOME/build-tools/*/aapt2 dump badging app/build/outputs/apk/debug/app-debug.apk | head -1` shows `versionCode='100' versionName='0.1.0'`
  - Result (2026-11-28): `assembleDebug` succeeded; `aapt2 dump badging` shows `versionCode='100' versionName='0.1.0'`. The parse-failure message was not exercised.

## 2. Android release signing

- [x] 2.1 Add the `release` signing config from `HUBBRIDGE_KEYSTORE_FILE` / `HUBBRIDGE_KEYSTORE_PASSWORD` / `HUBBRIDGE_KEY_ALIAS` / `HUBBRIDGE_KEY_PASSWORD`, with the debug fallback and a Gradle warning when unset (decision 2); verify `./gradlew assembleRelease` without the variables succeeds with the warning and `apksigner verify --print-certs` shows the debug certificate, and with a throwaway keystore made in the scratch dir (`keytool -genkeypair … -storepass` is fine for a throwaway) it shows that certificate instead
  - Result (2026-11-28): without the variables: Gradle warning, certificate `CN=Android Debug`. With a throwaway keystore: its certificate (`CN=throwaway`, digest equal to `keytool -list`). With the release key: SHA-256 `ada28626…bc29e535`, equal to the one in 2.2.
- [x] 2.2 Done on 2026-11-27 by the agent at Roman's request ("create the keystore yourself"): `~/.config/hubbridge/release.keystore` (PKCS12, RSA 4096, alias `hubbridge`, 100 years, key password = store password, mode 600), password in `~/.config/hubbridge/release.keystore.password` (mode 600) and in the login Keychain (service `HubBridge-Android-release-keystore`, account `hubbridge`); the four secrets are set on DeMoZ/HubBridge (`gh secret list` shows the names). Certificate SHA-256 `AD:A2:86:26:04:6F:1A:FF:A0:33:05:C1:A5:E7:34:15:1C:51:91:F6:3E:21:6F:84:A6:2A:A7:7C:BC:29:E5:35`. Local release builds read `HUBBRIDGE_KEYSTORE_FILE` / `HUBBRIDGE_KEYSTORE_PASSWORD` from these files; never print the password

## 3. macOS packaging script

- [ ] 3.1 Add `scripts/build-macos.sh` per decision 3 (options `--apk`, `--skip-tests`; build-and-copy order for the two binaries with the `cmp` guard; bundle, Info.plist + `plutil -lint`, icon with fallback, APK copy, ad-hoc `codesign` + verify, update zip with `ditto`, `.dmg` with Applications link, CLI tarball); `dist/` is git-ignored (add `/dist` to `.gitignore`); verify `scripts/build-macos.sh --apk android/app/build/outputs/apk/release/app-release.apk` produces `dist/HubBridge.app`, `dist/HubBridge-macOS.dmg`, `dist/HubBridge-0.1.0-macos-arm64.zip`, `dist/hubbridge-0.1.0-macos-arm64.tar.gz`
  - Result (2026-11-28): NOT verified on this Mac. The script builds the app, the update zip and `dist/cli`, then `hdiutil create` fails with `no mountable file systems` (3 script runs and 4 manual tries, also with `-fs HFS+`; `diskutil image create from` failed too). In the `diskarbitrationd` log a mount-approval client dissents from the mount (`0xF8DA000A`) until hdiutil gives up after about 2 minutes; the client was not identified (Surfshark Antivirus endpoint extension is active and UsbMemoryChecker was running; neither is proven). Attaching an image built without mounting fails the same way. One run with a temporary `hdiutil` shim on PATH (`makehybrid` + `convert`) finished with exit 0 and made all four outputs, so the rest of the script, including the tarball (`hubbridge`, `HubBridge.apk`, `LICENSE`), works. Re-run the real command when mounting works, or check the PR dry run (4.3). `-quiet` was left out of `hdiutil create` because it hid the error.
- [ ] 3.2 Check the bundle against the spec "macOS app bundle": `plutil -p dist/HubBridge.app/Contents/Info.plist` shows the bundle id, version 0.1.0, `LSMinimumSystemVersion` 11.0, `NSBonjourServices` with `_hubbridge._tcp` and the local-network text; `codesign --verify --deep --strict dist/HubBridge.app` passes; `file dist/HubBridge.app/Contents/MacOS/HubBridge` says arm64; `unzip -l` of the zip lists `HubBridge.app/Contents/Resources/HubBridge.apk`; `hdiutil attach -nobrowse -readonly` of the dmg shows `HubBridge.app` and `Applications` (detach afterwards)
  - Result (2026-11-28): all checks except the dmg one pass: Info.plist values as specified, `codesign --verify --deep --strict` OK (adhoc, identifier `com.demoz.hubbridge`), arm64, the zip lists `Contents/Resources/HubBridge.apk` (same bytes as the release APK). `hdiutil attach` of the dmg NOT verified (see 3.1).
- [ ] 3.3 Run the packaged app the way a user would: copy `dist/HubBridge.app` to a scratch folder, `xattr -w com.apple.quarantine "0081;$(printf %x $(date +%s));Safari;" <copy>` to simulate a browser download, open it and note the exact Gatekeeper dialog and the "Open Anyway" path on this macOS (for the docs in group 5); then with `HUBBRIDGE_HOME` pointed at a scratch dir (`open --env HUBBRIDGE_HOME=<scratch> <copy>`) confirm the Local Network prompt shows the description text and, after Allow, the simulator (`hubbridge-agent-sim --pin 123456 --state-dir <scratch>`) appears under "On this network"
  - Result (2026-11-28): observed on macOS 27.0.1, quarantined copy: dialog title `“HubBridge.app” Not Opened`, text `Apple could not verify “HubBridge.app” is free of malware that may harm your Mac or compromise your privacy.`, buttons `Move to Trash` and `Done` (`spctl`: rejected). The System Settings → Privacy & Security → Open Anyway step was NOT observed (needs clicks; the dialog was closed by ending its process). A second copy with the quarantine flag removed, started with a scratch `HUBBRIDGE_HOME`, showed `Allow “HubBridge” to find devices on local networks?` with the `NSLocalNetworkUsageDescription` text. Allow was NOT clicked (needs a person). With the prompt unanswered the window already listed the simulator under "On this network" (page text read through `HUBBRIDGE_DEV_SCRIPT`; the simulator runs on the same Mac), so this does not prove the after-Allow case with a phone. `screencapture -l` of the window itself came out blank although the page was rendered.
- [x] 3.4 Bundled APK in use: with a phone on USB (if one is attached) press "Install app" in the packaged window and confirm the toast names version 0.1.0; without a phone, run `<scratch>/cli/hubbridge install-apk` from the unpacked CLI archive with no phone and confirm it does not fail with "no HubBridge APK found" (if it checks the device first, run it with `HUBBRIDGE_LOG=debug` or with `--apk` omitted against a phone later; the point is that the bundled APK is picked); verify the CLI archive unpacks to `hubbridge`, `HubBridge.apk`, `LICENSE` and `./hubbridge --help` lists `discover`
  - Result (2026-11-28): no-phone path. A real phone was visible to adb, so the CLI ran with `ADB` pointed at a fake adb script (the real adb was never called by the CLI): it printed `Installing <unpacked>/HubBridge.apk (bundled) on Fake Phone …` and passed that file to `install`. The APK is 0.1.0 / versionCode 100. `./hubbridge --help` lists `discover`. The archive checked was made with the same `tar` command as the script; the shim run in 3.1 produced the same three entries. Nothing was installed on a real phone.

## 4. Release workflow

- [x] 4.1 Add `.github/workflows/release.yml` per decision 4 (jobs `android`, `macos`, `release`; tag check through `scripts/version.sh`; keystore step that fails on a tag and warns on a manual run when a secret is empty, without `set -x`; `apksigner verify`; the four files; notes file or generated notes; title `HubBridge vX.Y.Z`); verify `actionlint .github/workflows/release.yml` (install with `brew install actionlint` if missing) reports nothing
  - Result (2026-11-28): `actionlint` reports nothing. The release job and the strict tag/key checks also require a `push` event, so a manual run on a tag cannot publish. The keystore step was run locally: fails on push with empty secrets (names all four), warns on pull_request and workflow_dispatch.
- [x] 4.2 Add `docs/releases/v0.1.0.md` (decision 5: English + Russian "What's new" for the first release — Wi-Fi/ADB bridge, PIN pairing, desktop window, USB device list, serial bridge, in-app updates and Support if `desktop-updates-donate` is in the same release; then `## Install` and `## Установка` with the Gatekeeper steps observed in 3.3 and the APK note about debug builds); verify the headings are exactly `## What's new`, `## Что нового`, `## Install`, `## Установка` in that order (`rg -n '^## ' docs/releases/v0.1.0.md`)
  - Result (2026-11-28): the four headings are in the required order. Gatekeeper wording follows 3.3; the Open Anyway step is the standard macOS 15+ flow, not observed. Includes in-app updates and Support from `desktop-updates-donate`.
- [ ] 4.3 Dry run on the feature branch through the PR: `release.yml` also triggers on `pull_request` for the release files (design decision 4; a new workflow cannot be dispatched by hand until it is on the default branch). Push the branch, open the PR, then `gh run watch` the `Release` run of the PR; verify the run is green, the `release` job is skipped, its artifacts hold the four files (`gh run download <id> -D <scratch>/run` and list them), and `gh release list -R DeMoZ/HubBridge` is still empty. Opening the PR is the orchestrator's step — if you are the executor, stop after 4.2 is ready and report; the orchestrator pushes, opens the PR and does this check

## 5. Docs

- [x] 5.1 `README.md` and `README.ru.md`: new "Install" section first under the intro (download the `.dmg` from Releases, the Gatekeeper "Open Anyway" step and the `xattr` alternative, Local Network prompt, phone app from the window's "Install app" or the `.apk`), the CLI archive, a note that Windows runs from source only for now; status line updated (released); verify both READMEs have the same sections (`rg -n '^##' README.md README.ru.md`)
  - Result (2026-11-28): both READMEs have the same 8 sections in the same order (`README.ru.md` also gained How it works, Commands, Develop and License).
- [x] 5.2 New `docs/RELEASING.md` (developer docs; the repo has no CLAUDE.md and agent notes are never added to it): how to cut a release — bump `version` in `[workspace.package]` (Gradle follows), write `docs/releases/v<version>.md` (en + ru, Install sections last; the update window shows it), merge to main, then tag `v<version>` on main and push the tag; the four secrets and what happens without them; `scripts/build-macos.sh` for a local build; the HubBridge/hubbridge case collision; no Windows/Linux release files; verify the file names every file above and `docs/ARCHITECTURE.md` (docs list / layout) links to it
  - Result (2026-11-28): `docs/RELEASING.md` written; `docs/ARCHITECTURE.md` links to it.
- [x] 5.3 `docs/ROADMAP.md`: tick the `.app` packaging part of the desktop-window item (Info.plist keys done; tray, launch at login, disclaimed adb spawn stay open) and the Step 4 "Desktop release archives ship `HubBridge.apk`" part; "signed release builds" stays partly open (APK signed, macOS ad-hoc); `docs/ARCHITECTURE.md` repository layout: `scripts/`, `docs/releases/`, `.github/workflows/release.yml`; `docs/LIMITATIONS.md`: unsigned/not notarized macOS app, possible Local Network re-prompt after updates, adb server network access after quitting the packaged app, phone app signed with the release key (dev builds must be uninstalled once); verify each file mentions its new item (`rg -n "release.yml" docs/ARCHITECTURE.md`, `rg -n -i "notari" docs/LIMITATIONS.md`)
  - Result (2026-11-28): `rg` finds `release.yml` in ARCHITECTURE and the notarization item in LIMITATIONS (new section 5; the video latency section is now 6).

## 6. Integration checks

- [x] 6.1 Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`; verify all pass (this change touches no Rust code; the run guards against accidental edits)
  - Result (2026-11-28): fmt and clippy clean; `cargo test --workspace`: 188 passed, 0 failed.
- [ ] 6.2 Run `cargo check --target x86_64-pc-windows-msvc -p hubbridge-desktop`; verify it passes or fails only in the known `ring` C build (leave to CI), and after push `ci.yml` is green on macOS, Windows and the APK job
- [ ] 6.3 `git status` shows no `dist/`, keystore or `.so` files staged; verify `git diff --cached --stat` lists only the files of this change
  - Result (2026-11-28): nothing is staged (the executor runs no git writes) and `git status` shows no `dist/`, keystore or `.so` files. Check `git diff --cached --stat` at commit time.

## Workflow follow-up

- Review (fresh Opus subagent) against `specs/release-builds/spec.md` and this design.
- After review is clean and `desktop-updates-donate` is done: `/opsx:archive release-pipeline`,
  PR; merging to main is Roman's step.
- After the merge, with the secrets set: a manual run of `release.yml` on main (dry run) is
  green; then the agent runs `git tag v0.1.0 origin/main && git push origin v0.1.0` and watches
  the run.
- Verify the published release against the spec: four assets with digests
  (`gh api repos/DeMoZ/HubBridge/releases/latest --jq '.assets[] | [.name, .digest] | @tsv'`),
  body = `docs/releases/v0.1.0.md`; download the `.dmg` in a browser and repeat check 3.3 on the
  real download.
- At the first follow-up release (v0.1.1): install it over v0.1.0 on a phone (spec "Same key
  across releases") and note whether macOS asked for Local Network access again after the
  in-app update; update LIMITATIONS with the answer.
