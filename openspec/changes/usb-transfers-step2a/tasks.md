# Tasks

Groups 1–4 were implemented before OpenSpec was adopted in this repo (commits 164b507 and
6ffd18d on `feat/step2-usb-transfers`) and are checked against the specs here.

## 1. Protocol

- [x] 1.1 Append the USB request/reply/event messages, `Setup`, `UsbStatus` and the new error codes to `crates/proto`; verify `cargo test -p hubbridge-proto` passes (frame round trip)
- [x] 1.2 Add the flash-drive fixture (Bulk-Only, 512-byte bulk endpoints); verify its descriptors parse in the fixture helper
- [x] 1.3 Document the USB data messages in `docs/PROTOCOL.md`; verify the table lists every new request and reply

## 2. Agent core and backends

- [x] 2.1 Add `UsbBackend`/`UsbDevice` with synchronous queueing and async results; verify the loopback-order e2e test passes
- [x] 2.2 Session handling: per-session ownership, `Busy`/`NotOpen`, close on `CloseDevice`, session end and unplug with `DeviceClosed`; verify the e2e test `descriptors_loopback_and_errors`
- [x] 2.3 Fake backend: descriptors and strings, loopback, Bulk-Only flash drive with stall on short data; verify `mass_storage_read` runs in under 2 s
- [x] 2.4 nusb backend on a usbfs fd with one worker per endpoint, timeouts, clear halt; verify `cargo ndk -t arm64-v8a clippy -p hubbridge-agent -- -D warnings` is clean
- [x] 2.5 Android: JNI opener/closer in `crates/android`, Kotlin `onOpenDevice`/`onCloseDevice` in `BridgeService`; verify `./gradlew assembleDebug` succeeds
- [x] 2.6 Simulator registers the fakes (UART loopback, keyboard without permission, 32 MiB drive); verify `hubbridge usb storage 1004 --bench 32` against it

## 3. Host client and CLI

- [x] 3.1 `Client::submit`/`Pending`, USB methods, `AgentError` with the agent's code; verify the e2e tests use them for in-flight transfers
- [x] 3.2 Mass-storage client (INQUIRY, TEST UNIT READY, REQUEST SENSE, READ CAPACITY, READ(10), pipelined data phase, reset recovery, early CSW); verify byte-by-byte read check in `mass_storage_read`
- [x] 3.3 CLI `hubbridge usb descriptors <id>` and `hubbridge usb storage <id> [--bench]` with the permission hint; verify against the simulator (descriptors "identical", storage identify + read, no-permission message)
- [x] 3.4 Remember loopback addresses of paired agents; verify `hubbridge usb ...` without a target finds a simulator paired on 127.0.0.1

## 4. Roadmap

- [x] 4.1 Mark step 2a in `docs/ROADMAP.md`; verify the checklist matches what is built

## 5. Fix from the real-phone test (uncommitted)

- [x] 5.1 Enable nusb's `tokio` feature in `crates/agent/Cargo.toml`; verify on the phone that `hubbridge usb descriptors 1004` answers instead of timing out
- [x] 5.2 Answer a USB request whose task panics with `Error Internal`; add a fake device that panics on open and verify an e2e test gets `Internal` (spec: Failures inside the agent still answer)
- [x] 5.3 Log Rust panics to logcat on Android; verify `cargo ndk -t arm64-v8a clippy -p hubbridge-android -- -D warnings` is clean
- [x] 5.4 `--chunk-kib` / `--command-kib` for `usb storage`, short-packet check against each chunk's own size; verify the e2e tests pass and `--help` lists both options

## 6. Tests for spec scenarios not covered yet

- [x] 6.1 Oversized transfer (> 1 MiB) gets `Error Unsupported`; verify with an e2e test
- [x] 6.2 300 IN transfers in flight on the fake loopback (more than the 256 limit) are all answered; verify with an e2e test
- [x] 6.3 `MassStorage::open` on a non-drive (the CP2102 fixture) fails without opening the device; verify with an e2e test that the device can still be opened afterwards
- [x] 6.4 `ClearHalt` and `ResetDevice` on an open device answer `Done`; verify with an e2e test

## 7. Integration check on hardware

- [ ] 7.1 Rebuild and install the APK over Wi-Fi ADB (`hubbridge install-apk --serial <ip>:5555`), then run `hubbridge usb descriptors 1004` and `hubbridge usb storage 1004 --bench 32` on the phone with the flash drive on the hub; verify both succeed and record the speed in the PR
- [ ] 7.2 Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`; verify all pass, then commit with the OpenSpec change and push; verify CI on PR #5 is green

## Workflow follow-up

- Archive the change after PR #5 is merged.
