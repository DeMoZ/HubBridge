# Proposal

## Why

The phone already sits on a USB-C hub with a monitor on it, and the Mac talks to the phone over Wi-Fi or ADB.
Roman wants the Mac's picture on that monitor, carried by the phone: Mac → Wi-Fi → HubBridge on the phone →
external monitor. It is the first usable piece of roadmap step 3 ("PC → phone / hub display") and needs no
driver, no private API and no signing on either side.

Two small faults in the window are fixed at the same time: one phone shows two identical "Wi-Fi" chips in the
"USB and ADB" panel, and the "On this network" card says "offline" for the phone the window is connected to over
ADB.

## What Changes

- **Spike first (go/no-go).** On the real phone (Samsung SM-G970F, Android 12, monitor on the USB-C port, Samsung
  DeX active) find out whether HubBridge can show its own picture on the HDMI display, and under which conditions
  (DeX on or off, phone screen on or off, app in the foreground or not). Today's read-only `dumpsys` shows the
  HDMI output showing DeX's 1600×900 desktop and the "HDMI Screen" display (id 8, 1280×720@60, a presentation
  display) without content of its own. Mirroring is built only after a route is found; no route = stop and decide.
- **Phone displays.** The phone app watches Android's presentation displays and reports them (id, name, current
  resolution and refresh rate, supported modes, whether Samsung DeX is on) to the computer; hot-plug changes
  arrive as events. The desktop window gets a "Phone displays" panel, and the CLI gets `hubbridge displays`.
- **Mirror the Mac screen (macOS).** The Mac captures its main display with ScreenCaptureKit, scales it to fit
  the phone display's resolution (black bars, no stretching), encodes H.264 in hardware with VideoToolbox in
  low-latency mode and sends it to the phone. The phone decodes it with `MediaCodec` (low-latency mode where the
  decoder supports it) straight onto a surface on the external display. Start and Stop in the window and in
  `hubbridge mirror`, with a stats readout (frames per second, bitrate, frame delay, skipped frames, phone
  decoder). Stops cleanly, with the reason shown, when the monitor is unplugged, the phone app stops it, or the
  connection drops.
- **Video connection.** Frames travel on a second encrypted session to the same phone (an ordinary resumed
  session that attaches itself to the mirror stream), so video never queues in front of pings and USB requests.
  The Mac keeps at most two frames waiting for the phone's acknowledgement and skips captured frames rather than
  letting a delay build up. QUIC is not used yet.
- **Protocol.** New `Message` variants are appended only; hellos, key derivation and record framing do not
  change, so `PROTO_VERSION` stays 1. A host asks for displays only from a phone app new enough to know them,
  and the phone sends display events only to a host that asked, so old and new apps keep working together.
- **Screen Recording permission flow** on macOS: check before starting, ask once, and explain how to allow it
  (including that the ad-hoc signed app loses the grant on every update). The minimum macOS of the app and the
  CLI rises from 11.0 to 13.0, because ScreenCaptureKit (12.3+) is linked by the binaries.
- **Window fixes.** One chip per link kind (USB, Wi-Fi) per phone in "USB and ADB", however many adb transports
  stand behind it. The phone card in "On this network" shows the real link state: connected (over Wi-Fi or ADB)
  when the window is connected to it, "on the network" when mDNS sees it, and "not seen on the network" (with a
  hint about macOS Local Network access) instead of "offline" otherwise.
- **Testing without a phone:** the simulator gets fake displays, display hot-plug, a slow-decoder option and a
  file sink that writes the received H.264 stream; a synthetic frame source lets every protocol and back-pressure
  path run in CI on macOS and Windows. Real-hardware checks are listed in the tasks.
- Out of scope (later stages): a separate extended desktop (`CGVirtualDisplay`, private API), mirroring with the
  phone screen off or the app in the background unless the spike shows it works for free, automatic resume after
  a dropped connection, adaptive bitrate, HEVC, audio, QUIC datagrams, capture on Windows. The capture source is
  a ScreenCaptureKit display id, so a virtual display created later plugs into the same pipeline.

## Capabilities

### New Capabilities

- `phone-displays`: the phone's external (presentation) displays as the computer sees them: the list, its
  fields, hot-plug events, the opt-in rule for events, and how the window and the CLI show them.
- `screen-mirroring`: showing the Mac's screen on a phone display: starting and stopping, scaling and black bars,
  the video session and its encryption, back-pressure and keyframes, what ends a mirror and how the reason is
  shown, the stats readout, the Screen Recording permission flow, compatibility with older apps, and testing
  without a phone.

### Modified Capabilities

- `desktop-window-design`: "Status is never shown by color alone" changes its offline wording (the connected
  phone is never shown as offline; "not seen on the network" replaces "offline"), "Layout regions of the window"
  gains the "Phone displays" panel in the right column, and a new requirement keeps one link chip per kind for
  each phone in the "USB and ADB" panel.

## Impact

- `crates/proto`: appended messages and types (`DisplayInfo`, `VideoConfig`, `VideoFrame`, `VideoAck`, error
  codes); round-trip and "old variants keep their index" tests.
- `crates/agent`: display registry, mirror stream registry, the media-session mode, a `VideoSink` trait with a
  fake sink; `crates/agent-sim`: `--display`, `--display-churn`, `--decode-delay-ms`, `--video-out`.
- `crates/android` (JNI) and the Android app: `DisplayWatcher.kt`, the decoder and surface on the external
  display (route chosen by the spike), JSON field names matched by a Rust test; notification text while
  mirroring.
- `crates/host`: `mirror` module (fit, Annex B conversion, pacing window, stats, synthetic source; macOS
  ScreenCaptureKit + VideoToolbox behind `cfg(target_os = "macos")`), `hubbridge displays` and
  `hubbridge mirror`, `mirror_e2e.rs`. New macOS-only dependencies from the objc2 family already in the lock file
  (`objc2-screen-capture-kit`, `objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video`, 0.3.x, MSRV 1.71).
- `crates/desktop`: `MirrorView` and display list in the model, new requests, the "Phone displays" panel, the
  chip grouping and agent link state, permission sheet; mirroring stops before an update quits the app.
- `scripts/build-macos.sh`: `LSMinimumSystemVersion` and `MACOSX_DEPLOYMENT_TARGET` 13.0.
- Docs: `docs/PROTOCOL.md`, `docs/ARCHITECTURE.md`, `docs/ROADMAP.md` (step 3), `docs/LIMITATIONS.md` (§2, §4,
  §5, §6).
- Windows keeps compiling and passing tests; mirroring there reports "needs macOS for now".
