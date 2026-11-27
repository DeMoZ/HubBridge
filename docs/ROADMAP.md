# Roadmap

## Step 1 — MVP: protocol, pairing, metadata ✅ (this PR)

- [x] Rust workspace: `proto`, `link`, `agent`, `android` (JNI), `agent-sim`, `host`.
- [x] Pairing with a 6-digit PIN (SPAKE2), encrypted sessions (Noise NNpsk0), key stores on
      both sides, attempt limits.
- [x] Android app: foreground service (`connectedDevice`), wake lock + Wi-Fi low-latency
      lock, NSD announce, USB attach/detach/permission, device list with raw descriptors.
- [x] Wi-Fi/LAN (dual-stack listen) and ADB-only (localhost) modes; `hubbridge adb`
      sets up/reuses `adb forward`.
- [x] Host CLI: `discover`, `pair`, `info`, `devices [--json]`, `grant`, `ping`, `watch`
      (events + RTT + reconnect), `agents`, `forget`, `install-apk` (install/update the
      app on a USB-connected phone and open it).
- [x] Descriptor parser, simulator with fake CP2102 + HID keyboard, unit + e2e tests, CI.

Acceptance on hardware: phone + hub + CP2102/CH340 adapter → `hubbridge devices` shows the
adapter with its endpoints over Wi-Fi and over `adb forward`; unplugging it shows in
`hubbridge watch` within a second.

## Step 2 — USB data forwarding

Step 2b ✅ (serial bridge, tested with fakes in CI; no hardware check yet).

Step 2a ✅ (generic transfers, tested with fakes; real-phone test pending):
- [x] Protocol: open/close, claim/release (kernel driver detach), alternate settings,
      control, bulk/interrupt transfers with many in flight in order, clear halt, reset.
- [x] Agent: nusb on the Android usbfs fd, one worker per endpoint, timeouts; device
      ownership per session; fake backend (loopback, Bulk-Only flash drive).
- [x] Host: client API, mass-storage client (INQUIRY / capacity / READ(10) with pipelined
      data phase), `hubbridge usb descriptors|storage [--bench]`.

1. `crates/usbio` (Android): open device by fd (`UsbDeviceConnection.getFileDescriptor()`
   passed through JNI), claim/release interfaces (detach kernel driver), control transfers,
   async URB queue for bulk/interrupt with configurable depth, cancellation, reset.
2. Protocol: `OpenDevice`, `ClaimInterface`, `ControlTransfer`, `SubmitUrb`/`UrbComplete`,
   flow control (per-endpoint credit), binary payloads after the frame header.
3. First consumer, no OS driver needed: **serial bridge** — CDC-ACM, CP210x, FTDI, CH34x.
   Built (step 2b, tested with fakes; the drivers run on the host over the generic
   transfers, the agent stays chip-agnostic): `hubbridge serial` with a PTY (macOS),
   raw TCP and RFC 2217 (macOS and Windows), `--bench-echo` / `--bench-request`, desktop window
   card. Tested with fakes in CI; no hardware check yet (no USB-serial device on hand). The
   echo and throughput measurement with a real device stays open (115200 and 3 Mbaud over
   Wi-Fi); it decides whether a generic agent-side IN stream is needed.
   Cancel-safe sends in the host client (#7): done.
4. **USB/IP**: `crates/usbip` codec, agent side executes `CMD_SUBMIT` on usbfs, host side
   localhost server on 3240 tunnelled through the session; attach with
   usbip-win2 on Windows. Test: HID keyboard, Xbox-style gamepad, USB-UART, flash drive.
5. HID proxy for macOS (keyboard/mouse via `CGEvent`; generic HID via HIDDriverKit later).

## Step 3 — Video / virtual display

1. Media connection: QUIC (quinn) datagrams over LAN, TCP over adb; frame header with
   timestamps, keyframe requests, stats (FPS, bitrate, encode/network/decode latency).
2. (b) Capture dongle → PC: UVC on the phone (Camera2 external camera if present, else
   user-space UVC on usbfs), MJPEG pass-through or `MediaCodec` H.264; host window with
   live stats; then virtual camera (Windows 11 `MFCreateVirtualCamera`, macOS CMIO
   extension).
3. (a) PC → phone: Windows IddCx virtual monitor (test-signed for dev; evaluate signed
   Virtual Display Driver first), macOS `CGVirtualDisplay`; capture + hardware encode;
   phone decodes with `MediaCodec` low-latency onto its screen or a `Presentation` on the
   hub's HDMI output.
4. Measure glass-to-glass latency with a flashing test pattern + phone camera.

## Desktop window (pulled forward from step 4) ✅

- [x] `crates/desktop` (binary `HubBridge`, tao + wry): agents on the LAN, pairing with the
      PIN, connect by agent / IP / adb, live status and RTT graph, auto-connect and reconnect,
      USB devices with details and "Ask phone".
- [x] adb panel based on UniversalKM's approach: adb found in PATH and common install dirs,
      transports grouped per phone by `ro.serialno`, Install/Reinstall app, Connect via ADB,
      ADB over Wi-Fi (`tcpip 5555` + `connect`, saved and reconnected every ~6 s, "plug the
      cable once" after a phone restart), wireless-debugging pairing by code.
- [x] Redesign "Ambient Glass": dark and light themes following the system, one accent,
      link island in the top bar, AA text contrast (guarded by a test), keyboard focus, reduced
      motion / contrast / transparency overrides implemented, new app icon (`crates/desktop/assets`).
- [ ] Verify reduced motion / increased contrast overrides with the real macOS settings (tech debt
      from the redesign).
- [ ] Android control screen in the desktop's visual language (a new view layer; the launcher
      icon already uses the new mark).
- [ ] Tray icon, launch at login, `.app` / installer packaging (macOS needs
      `NSLocalNetworkUsageDescription` + `NSBonjourServices = _hubbridge._tcp` in Info.plist,
      and the adb server should be spawned "disclaimed" so it keeps network access after the
      app quits — see UniversalKM `DisclaimedSpawn.swift`).
- [ ] More languages than en/ru; settings page.

## Step 4 — Reliability and UX

- Reconnect with exponential backoff and session resumption of open devices; buffering and
  back-pressure per endpoint.
- Adaptive video: pick codec (H.264/HEVC/MJPEG), resolution and bitrate from measured
  RTT/loss/throughput.
- Tray app (tao + wry + tray-icon): agents, devices with attach/detach toggles, RTT/link
  graph, logs; keys in Keychain / Credential Manager.
- **"Install on phone" button** in the desktop app: lists phones from `adb devices -l`,
  installs/updates the bundled APK and opens it (`hubbridge_host::install::install`,
  already behind `hubbridge install-apk`). Desktop release archives ship `HubBridge.apk`
  next to the executable (macOS: `Contents/Resources`); later, download the matching APK
  from the GitHub release when it is not bundled.
- Structured logs on both ends with export; diagnostic bundle.
- Android: battery-optimization exemption flow, Quick Settings tile, "keep screen on" mode,
  PIN-less pairing over ADB (the host pushes a one-time secret with `adb shell`).
- Updates and Support button like the other DeMoZ apps; signed release builds.
