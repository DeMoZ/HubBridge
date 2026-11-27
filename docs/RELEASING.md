# Releasing HubBridge

A `v*` tag runs `.github/workflows/release.yml`, which builds and publishes a GitHub release. There is one version
number: `version` in `[workspace.package]` of `Cargo.toml`. `scripts/version.sh` prints it, and
`android/app/build.gradle.kts` reads it as `versionName`, with `versionCode` = major × 10000 + minor × 100 + patch.

## Cut a release

1. Bump `version` in `[workspace.package]` of `Cargo.toml` (Gradle follows) and update `Cargo.lock`
   (`cargo check` does it).
2. Write `docs/releases/v<version>.md`: `## What's new`, then `## Что нового`, then `## Install` and `## Установка`
   last. It becomes the release body, and the update window of the app shows it up to the first Install heading.
   Without the file GitHub generates the notes.
3. Merge to `main`.
4. Tag `v<version>` on `main` and push the tag.

The tag must be `v` + the version in `Cargo.toml`, otherwise the build stops and nothing is published.

## What is published

| File | Content |
|---|---|
| `HubBridge-macOS-v<version>.dmg` | `HubBridge.app` (Apple Silicon, ad-hoc signed) and an Applications link |
| `HubBridge-<version>-macos-arm64.zip` | the same app, used by the in-app update |
| `HubBridge-<version>.apk` | the phone app, signed with the release key |
| `hubbridge-<version>-macos-arm64.tar.gz` | the `hubbridge` CLI, `HubBridge.apk` and `LICENSE` |

No Windows and no Linux release files are published. Windows is compiled and tested in CI (`ci.yml`); the Android
build and the publish step run on a hosted Ubuntu runner, which does not make a Linux build of HubBridge.

## Signing the APK

Every release APK must be signed with the same key. A phone cannot update an app to one signed with another key
without uninstalling it, which forgets the paired computers. Keep an offline backup of the keystore and its password.

The workflow reads four repository secrets:

- `ANDROID_KEYSTORE_BASE64` (the keystore file, base64)
- `ANDROID_KEYSTORE_PASSWORD`
- `ANDROID_KEY_ALIAS`
- `ANDROID_KEY_PASSWORD`

Without any of them a tag build fails and names the missing ones. A dry run only warns and signs with the debug key.

A local `./gradlew assembleRelease` reads `HUBBRIDGE_KEYSTORE_FILE`, `HUBBRIDGE_KEYSTORE_PASSWORD`,
`HUBBRIDGE_KEY_ALIAS` and `HUBBRIDGE_KEY_PASSWORD`. If they are not all set, it signs with the debug key and prints a
warning. Pass passwords through the environment only, not as Gradle `-P` arguments.

## Dry run

The workflow also runs, without publishing, on a pull request that changes the release workflow,
`scripts/build-macos.sh`, `scripts/version.sh`, `android/app/build.gradle.kts` or `docs/releases/`, and when started
by hand (GitHub allows that once the workflow is on `main`). The built files are the artifacts of the run.

## Local build of the macOS files

```bash
scripts/build-macos.sh --apk android/app/build/outputs/apk/release/app-release.apk
```

It runs the tests (`--skip-tests` leaves them out) and writes `dist/HubBridge.app`, `dist/HubBridge-macOS.dmg` (the
release job adds `-v<version>` to this name), the update zip and the CLI archive. Without `--apk` it takes the release
APK built in the checkout, if there is one; with no APK it warns and "Install app" has nothing to install.

The window binary `HubBridge` and the CLI binary `hubbridge` have names that differ only in case. On a
case-insensitive disk (the macOS default, also on GitHub's runners) `target/<target>/release/HubBridge` and
`target/<target>/release/hubbridge` are the same file, and building one overwrites the other. The script therefore
copies each binary right after building it and fails if the two copies are identical. Keep that order.

If `hdiutil create` fails with "no mountable file systems", something on the Mac is refusing to mount new disk images
(`hdiutil info` lists images left attached by the failed run; detach them with `hdiutil detach`).
