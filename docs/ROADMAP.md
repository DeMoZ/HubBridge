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

### Step 3a — Mac screen → phone monitor (mirror)

OpenSpec change `mac-screen-to-monitor`.

- [x] Phone display spike on a real phone (2026-11-28): a `Presentation` from the foreground
      service works with Samsung DeX off, phone unlocked and screen on (LIMITATIONS §2).
- [x] Protocol: display list and events, mirror start/stop, video frames, acknowledgements,
      status ([PROTOCOL.md](PROTOCOL.md)); agent streams and video sessions; simulator with
      fake displays.
- [x] Media connection: a second TCP session now (Wi-Fi and ADB); QUIC datagrams are a later
      option. Back-pressure window, keyframe requests, stats (FPS, bitrate, delay, phone
      decoder); tested without a phone.
- [x] macOS: ScreenCaptureKit capture, VideoToolbox low-latency H.264, Screen Recording
      permission; `hubbridge displays` and `hubbridge mirror`.
- [ ] Android: display list, `MediaCodec` low-latency decoding onto the monitor.
- [ ] Desktop window: "Phone displays" panel with Mirror/Stop.
- [ ] Checks on real hardware, measured delay (LIMITATIONS §6).

### Later

1. (b) Capture dongle → PC: UVC on the phone (Camera2 external camera if present, else
   user-space UVC on usbfs), MJPEG pass-through or `MediaCodec` H.264; host window with
   live stats; then virtual camera (Windows 11 `MFCreateVirtualCamera`, macOS CMIO
   extension).
2. (a) PC → phone as an extended desktop: Windows IddCx virtual monitor (test-signed for dev;
   evaluate signed Virtual Display Driver first), macOS `CGVirtualDisplay`; the virtual
   display feeds the same capture and encode pipeline. Windows capture for mirroring.
3. Measure glass-to-glass latency with a flashing test pattern + phone camera.

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
- [x] `.app` packaging for macOS (`scripts/build-macos.sh`): `.dmg`, update zip, icon,
      `NSLocalNetworkUsageDescription` and `NSBonjourServices = _hubbridge._tcp` in Info.plist,
      published by `release.yml` on a version tag.
- [ ] Tray icon, launch at login, and spawning the adb server "disclaimed" so it keeps network
      access after the app quits (see UniversalKM `DisclaimedSpawn.swift`).
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
  already behind `hubbridge install-apk`).
  - [x] Desktop release archives ship `HubBridge.apk` next to the executable (macOS:
        `Contents/Resources`).
  - [ ] Later, download the matching APK from the GitHub release when it is not bundled.
- Structured logs on both ends with export; diagnostic bundle.
- Android: battery-optimization exemption flow, Quick Settings tile, "keep screen on" mode,
  PIN-less pairing over ADB (the host pushes a one-time secret with `adb shell`).
- [x] Updates and Support button like the other DeMoZ apps.
- [ ] Before publishing a Windows file: make the Windows replace script back up the installed
      files and restore them if the copy fails (tech debt from the updater).
- Signed release builds: the APK is signed with the release key; the macOS app is only ad-hoc
  signed (Developer ID signing and notarization are open).
