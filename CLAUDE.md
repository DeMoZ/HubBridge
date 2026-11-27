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

## Desktop window (`crates/desktop`)

- tao + wry like UsbMemoryChecker: page `src/ui/index.html` (`include_str!`, `/*PLATFORM*/`
  replaced), controller `src/app.rs` on tokio; the page gets the whole `Model` as JSON
  (`window.onState`) after each change, toasts via `window.onToast`; requests are
  `{"action": ...}` JSON matching `app::Request` (serde tag, snake_case).
- **Never declare a top-level `ipc` in the page script**: wry owns the non-configurable
  global `window.ipc`, and WebKit then rejects the whole script (empty window, no error).
  The helper is `send()`.
- The WebView lives only on the main thread; other threads use the `EventLoopProxy`.
- Checks: `HUBBRIDGE_DEV_SCRIPT='js'` runs in the page 3 s after start;
  `HUBBRIDGE_HOME=<scratch dir>` gives a fresh key store. Screenshot only the window:
  get its id with a CGWindowList Swift snippet (owner name "HubBridge"), then
  `screencapture -l <id> -o -x shot.png`.
- adb handling follows UniversalKM (`~/Git/UniversalKM/Sources/UniversalKM/ADBClient.swift`,
  `AndroidHIDTransport.swift`): the user's adb and shared server, timeouts on every command,
  `ro.serialno` to merge USB and Wi-Fi transports of one phone.

## Rules

- The protocol is versioned (`PROTO_VERSION`); only append `Message` variants. Changing
  hellos, key derivation or record framing = bump the version and update PROTOCOL.md.
- Key files (`host.json`, `agent-state.json`) hold secrets: never print their contents;
  `Secret`'s Debug is redacted on purpose.
- Kotlin ↔ Rust is JSON over JNI; field names in `UsbScanner.kt` must match
  `AndroidDevice` / `ConfigInfo` in `crates/android` and `crates/proto` (test
  `parses_scanner_json`).
- Desktop platforms are macOS and Windows; the phone is Android. There is **no Linux
  platform**: no Linux code, no Linux CI job, no Linux releases. Windows is supported in code
  (CI checks it); no Windows release builds unless Roman asks.
