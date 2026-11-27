# HubBridge — notes for agents

Android phone + USB hub → bridge to a desktop. Design: docs/ARCHITECTURE.md, limits:
docs/LIMITATIONS.md, wire format: docs/PROTOCOL.md, plan: docs/ROADMAP.md. Keep these in
sync when the protocol or the scope changes.

## Build and test

- Rust: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all` (max_width 120). Edition 2024, MSRV 1.88 (let-chains).
- Android core only: `cargo ndk -t arm64-v8a -P 26 build -p hubbridge-android --release`
  (needs `ANDROID_NDK_HOME`).
- APK: `cd android && ./gradlew assembleDebug` — the `cargoNdk` task builds the .so for
  arm64-v8a, armeabi-v7a, x86_64 into `app/src/main/jniLibs` (git-ignored); `-PskipCargo`
  skips it. On this Mac: `ANDROID_HOME=/opt/homebrew/share/android-commandlinetools`,
  `JAVA_HOME=$(/usr/libexec/java_home -v 25)`.
- Without a phone: `hubbridge-agent-sim --pin 123456 [--churn 3]` + the `hubbridge` CLI.
  Use `HUBBRIDGE_HOME=<scratch dir>` and `--state-dir <scratch dir>` so tests do not touch
  the real key stores, and a non-default `--listen 0.0.0.0:<port>` if 47800 is busy.

## Rules

- The protocol is versioned (`PROTO_VERSION`); only append `Message` variants. Changing
  hellos, key derivation or record framing = bump the version and update PROTOCOL.md.
- Key files (`host.json`, `agent-state.json`) hold secrets: never print their contents;
  `Secret`'s Debug is redacted on purpose.
- Kotlin ↔ Rust is JSON over JNI; field names in `UsbScanner.kt` must match
  `AndroidDevice` / `ConfigInfo` in `crates/android` and `crates/proto` (test
  `parses_scanner_json`).
- Windows is a supported host platform in code (CI checks it); no Windows release builds
  unless Roman asks.
