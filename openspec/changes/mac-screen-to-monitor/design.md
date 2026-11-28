# Design

## Context

### Measured on the real phone today (read-only adb, 2026-11-28)

Samsung SM-G970F (Galaxy S10e, Exynos 9820), Android 12 / API 31, monitor on the USB-C port without a hub,
reached as `adb -s 192.168.1.108:5555` (a second transport, `adb-R28M303RHET-LfTN6A._adb-tls-connect._tcp.`, is
the same phone). The installed app is a **debuggable** build, versionName 0.1.0, so a new debug build installs in
place without forgetting the paired computers (as long as it is signed with this Mac's debug key).

- `dumpsys display`, logical displays: 0 "Built-in Screen" 1080×2280; 2 "Desktop" (virtual, owner `android`,
  1600×900, `mDexEnabled=true`, own display group 2); 8 "HDMI Screen" (`local:1`, EXTERNAL, 1280×720@60, modes
  1280×720@60, 720×480@59.94, 640×480@59.94; FLAG_SECURE, FLAG_SUPPORTS_PROTECTED_BUFFERS, FLAG_PRESENTATION,
  FLAG_TRUSTED; display group 0 after override).
- **The physical HDMI device `local:1` shows layer stack 2** (`mCurrentLayerStack=2`, layer-stack rect
  1600×900, display rect 1280×720): the monitor shows Samsung DeX's desktop, scaled. Logical display 8 has
  layer stack 8 and `mHasContent=false`. Display #2 runs `com.sec.android.app.desktoplauncher`; display #8 has no
  activities.
- Decoders (`/vendor/etc/media_codecs.xml`): `OMX.Exynos.avc.dec` (video/avc) with features `adaptive-playback`
  and **`low-latency`**, sizes up to 8192×8192, 1280×720 measured at about 800 frames/s.
- The agent listens on `[::]:47800` (Wi-Fi/LAN mode); the desktop window is connected through `adb forward`
  (loopback session visible on the phone).
- Mac side: system Bonjour (`dns-sd -B _hubbridge._tcp`) and the `hubbridge discover` CLI from the terminal both
  see the phone (instance `HubBridge-PhoneXXX`, port 47800, TXT `v=1 id=… name=PhoneXXX`) at the same moment
  the packaged `/Applications/HubBridge.app` (running, ad-hoc signed) shows the phone as "offline".

### Platform facts checked in sources (AOSP android12-release, Apple SDK headers and docs)

- `Presentation` on API 31 builds its own window context (`createDisplayContext(display)
  .createWindowContext(TYPE_PRESENTATION)`) and needs no activity token on a public presentation display, so
  **a foreground service context can show it**. It is cancelled when its display is removed; it is not hidden
  when an activity stops.
- `DisplayManagerService.configureDisplayLocked`: a display device mirrors another display only while its
  logical display has no content; once a visible window gives display 8 content, the device shows layer stack 8.
  This is why the HDMI device mirrors DeX today (display 8 is empty). Samsung may override it: **unverified**.
- While the **keyguard** (lock screen) shows on the phone, app content on secondary displays does not count as
  content: the external display falls back to mirroring. In stock Android 12 the external display is in the
  default display's power group: **phone screen off = HDMI off**. `FLAG_KEEP_SCREEN_ON` on a visible window
  (also one on the external display) keeps both on. DeX may differ: **unverified**.
- Launching our own activity on a public trusted display (`ActivityOptions.setLaunchDisplayId`) is allowed;
  background-activity-launch rules apply (a foreground service alone is not an exemption).
- `MediaFormat.KEY_LOW_LATENCY` and `FEATURE_LowLatency`: API 30. `KEY_PRIORITY` 0 = realtime.
- ScreenCaptureKit (`SCStream`, `SCShareableContent`, `minimumFrameInterval`, `queueDepth`, `showsCursor`):
  macOS 12.3+. Frames arrive only when the display changed (`SCFrameStatus.idle` otherwise). Pixel formats
  include `420v` (`kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`), which VideoToolbox takes without
  conversion. From a command-line tool, call `CGMainDisplayID()` once before any ScreenCaptureKit call
  (otherwise a `CGS_REQUIRE_INIT` assertion; forum fact, not documented).
- `CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`: macOS 10.15+. After the user allows the
  app, macOS offers "Quit & Reopen"; the result is often stale until relaunch. macOS 15+ asks again about once a
  month. TCC identifies the app by its designated requirement; an **ad-hoc signed app has no stable one, so every
  new build or update loses the grant**.
- VideoToolbox: `kVTVideoEncoderSpecification_EnableLowLatencyRateControl` macOS 11.3+, H.264 only, no frame
  reordering, infinite GOP (keyframes only when forced). The header says "High profiles only", WWDC21 shows
  Constrained Baseline and Constrained High; `kVTProfileLevel_H264_ConstrainedHigh_AutoLevel` is macOS 12.0+.
  `kVTEncodeFrameOptionKey_ForceKeyFrame` 10.8+.
- `objc2-*` framework crates link their framework strongly (`#[link(kind = "framework")]`). ld64 turns it into a
  weak load when `-weak_framework` is also passed (checked with this Mac's linker), but every newer symbol used
  would still be a strong import.

### Codebase context

- Protocol: `crates/proto/src/lib.rs` (`Message` is append-only, postcard encodes the variant index; an old peer
  that receives an unknown variant fails to decode the frame and **its session ends**). `crates/link` frames
  messages up to 16 MiB as 65535-byte Noise records; unchanged here.
- Agent: `crates/agent/src/lib.rs` — `handle_conn` → `run_session` with one broadcast `events` channel shared by
  all sessions, `dispatch` answers or defers (`Dispatch::Later`), idle timeout 20 s on frames from the host,
  `MAX_SESSIONS` 8, `SessionStatus` is JSON for Kotlin (not wire).
- Android: `crates/android/src/lib.rs` (JNI, JSON in, `JavaCallbacks` for calls into Kotlin), `BridgeService.kt`
  (foreground service `connectedDevice`, notification, callbacks), `MainActivity.kt` (lists sessions; would count
  a video session as a second computer), `UsbScanner.kt` (JSON field names matched by `parses_scanner_json`).
- Host: `crates/host/src/client.rs` (`Client::connect`, one writer task, 64-frame queue, events as
  `ClientEvent`), `main.rs` (CLI), `adb.rs` (`is_network_serial`: `ip:port` and `._adb-tls-connect.`).
- Desktop: `crates/desktop/src/app.rs` (`Model` pushed as JSON; `poll_adb` groups transports by `ro.serialno`
  into `PhoneView.transports`; `refresh_agents` sets `AgentView.online` only from the mDNS browser; the connection
  state is never consulted), `ui/index.html` (`renderPhones` draws one chip per transport, `renderAgents` draws
  "online"/"offline" from `a.online`), `main.rs` page tests (no top-level `ipc`, contrast, `put()` focus hosts,
  translations).

## Goals / Non-Goals

**Goals:**

- A picture of the Mac on the phone's monitor as early as possible, through the smallest pipeline that is
  reliable: one codec, one transport, one capture source, fixed presets, manual start.
- Every risky assumption is tested before code depends on it (phone display ownership first, Mac capture and
  encode second), and every protocol and back-pressure path is testable in CI without a phone or Screen
  Recording permission.
- Nothing in the design blocks a later extended desktop: the capture source is "a ScreenCaptureKit display",
  so a `CGVirtualDisplay` later is a different display id, not a new pipeline.

**Non-Goals:**

- Mirroring with the phone locked or its screen off (stock Android cannot; DeX is checked by the spike only).
- Automatic resume after a reconnect, adaptive bitrate, HEVC, audio, QUIC, Windows capture, choosing a Mac
  display other than the main one, touch/mouse input back from the monitor.

## Decisions

### D1. Spike on the phone first, with a go/no-go gate

Code that depends on owning the HDMI picture waits for a spike on the real phone. The spike is a small debug-only
addition to the Android app (`DisplaySpike.kt`, started by `adb shell am start -n
com.demoz.hubbridge/.MainActivity --es spike <route>` and honored only when the app is debuggable) that shows an
animated test pattern (moving bar, frame counter, display size) on display 8, logs `DisplayListener` events, and
logs the decoder it would pick for `video/avc` with `KEY_LOW_LATENCY`. Roman looks at the monitor; the executor
runs the adb commands and reads logcat.

Routes, tried in this order (stop at the first that passes the gate):

| Route | How | Needs |
|---|---|---|
| S1 | `Presentation(serviceContext, display8)` from `BridgeService`, `FLAG_KEEP_SCREEN_ON` on its window | nothing |
| S2 | S1 after Roman switches the monitor from DeX to "Screen mirroring" (Samsung quick panel) | user setting |
| S3 | our own full-screen activity on display 8 via `ActivityOptions.setLaunchDisplayId(8)`, started from the visible `MainActivity` | app visible at start |
| S4 | our activity on the DeX display 2, maximized (picture inside DeX, scaled 1600×900 → 1280×720) | DeX on |

For the passing route, record in the spike table (appended to this file by the executor): picture visible with
DeX on / off; phone app in the foreground / in the background (Home); another app full-screen on the phone; phone
locked (power button, then on, lock screen showing); phone screen off; monitor unplugged and replugged (events,
Presentation cancelled); what the monitor shows after the window is removed. Gate: **GO** when one route shows the
pattern on the monitor with the phone unlocked and screen on, whatever app is on the phone screen (S1/S2) or at
least while HubBridge is open (S3/S4). **NO-GO** when none does: stop the change and report to Roman (the options
left are the phone's own screen, or DeX window mode).

The rest of the design assumes **S1** (it is what AOSP predicts and needs no permission and no foreground
activity). If the spike picks S2, the only additions are a phone-side hint text and the DeX flag shown in the
window ("switch the monitor to Screen mirroring"); S3 adds the rule "HubBridge must be open on the phone when
mirroring starts" (start refused with that reason otherwise); S4 changes the target display to the DeX display.
The Kotlin side keeps one `MirrorSurface` seam (shows a `SurfaceView` on a display, reports surface ready / lost)
so the route is one class.

*Alternatives:* build first and test at the end (rejected: the measured layer stack says the monitor currently
shows DeX, so the whole feature might be invisible); `TYPE_APPLICATION_OVERLAY` window (rejected for now: needs
the "Display over other apps" permission and gives nothing S1 lacks, if S1 works).

### D2. Capture with ScreenCaptureKit through the objc2 crates

`objc2-screen-capture-kit` 0.3 with `block2` 0.6 and `dispatch2` 0.3 (all MSRV 1.71, same objc2 0.6 family
already in `Cargo.lock` through tao/wry), macOS-only target dependencies of `hubbridge-host`. The stream output
delegate is an `objc2::define_class!` class implementing `SCStreamOutput`, called on our own serial dispatch
queue. Configuration: the main display (`CGMainDisplayID()`, also needed for the CLI), `SCContentFilter` of that
display with no excluded windows, `width`/`height` = the fitted stream size (D5) so ScreenCaptureKit scales on the
GPU, `pixelFormat` = `420v`, `colorMatrix` BT.709, `minimumFrameInterval` = 1/fps, `queueDepth` = 3,
`showsCursor` = true. Only `SCFrameStatus.complete` samples are passed on; the last complete pixel buffer is kept
for keyframe requests on a still screen.

*Alternatives:* the `screencapturekit` crate 12.x (MSRV 1.82, fits, but a separate binding layer and a second way
of doing objc; rejected for one ecosystem); `CGDisplayStream` (deprecated since macOS 14); a Swift helper binary
(another build tool and process).

### D3. Minimum macOS 13.0 instead of 11.0

The capture and encoder crates link ScreenCaptureKit (12.3+) strongly and use VideoToolbox constants from 12.0
(`ConstrainedHigh_AutoLevel`). Raising `LSMinimumSystemVersion` and `MACOSX_DEPLOYMENT_TARGET` to 13.0 removes
every weak-linking concern; the release is Apple Silicon only, and every Apple Silicon Mac can run 13+.
13.0 rather than 12.3 because ScreenCaptureKit on 12.x had known leaks and crashes and nothing is gained.

*Alternative kept on file:* stay at 11.0 with `-weak_framework ScreenCaptureKit` (verified to produce
`LC_LOAD_WEAK_DYLIB`), a runtime version check, and only pre-11.0 VideoToolbox symbols or literal key strings.
More code paths, no tester for them. (Roman's decision; the default is 13.0.)

### D4. H.264 with VideoToolbox in low-latency mode; MediaCodec low-latency decode on the phone

Encoder: `VTCompressionSession` with encoder specification `EnableLowLatencyRateControl = true`, `RealTime =
true`, `AllowFrameReordering = false`, profile `ConstrainedHigh_AutoLevel` (falls back to `High_AutoLevel` if the
session refuses it; the fallback is logged), `AverageBitRate` from the preset, `ExpectedFrameRate` = fps, BT.709
color tags so the phone decodes colors right. Output is AVCC (length-prefixed NAL units) with SPS/PPS in the format
description; a pure function converts each sample to Annex B (start codes) and puts SPS and PPS in front of every
keyframe. A separate config frame (SPS + PPS only) is sent first and whenever the parameter sets change. Keyframes
are forced at start, on the phone's request, and every 10 s (low-latency mode has no GOP of its own), at most two
forced per second. Encoder drops (`kVTEncodeInfo_FrameDropped`) are counted and do not take a sequence number.

Presets: 30 fps (default) or 60 fps; bitrate = 0.25 bit per pixel per frame of the stream size, between 2 and
20 Mbit/s (1108×720@30 ≈ 6 Mbit/s); `hubbridge mirror --bitrate` overrides.

Decoder: `MediaCodecList(REGULAR_CODECS).findDecoderForFormat` for `video/avc` at the stream size (on the S10e
`OMX.Exynos.avc.dec`), `KEY_LOW_LATENCY = 1` only when the codec reports `FEATURE_LowLatency`, `KEY_PRIORITY = 0`,
output to the `SurfaceView`'s surface with `setFixedSize` = stream size (the display hardware scales). Config
frames are queued with `BUFFER_FLAG_CODEC_CONFIG`. Input is queued from the agent's sink thread
(`dequeueInputBuffer` with a 500 ms timeout); a separate output thread releases every decoded buffer for rendering
at once, and when several are ready renders only the newest (counted as dropped by the phone).

*Why not HEVC:* Exynos decodes it, but H.264 is the safest common denominator and the low-latency rate control
exists only for H.264. *Why not MJPEG/raw:* 5–10× the bandwidth for the same picture.

### D5. Fit, black bars and size limits

`fit(src_w, src_h, dst_w, dst_h)`: scale = min(dst_w/src_w, dst_h/src_h, 1920/src_w, 1080/src_h), sizes rounded
down to even numbers. `dst` is the phone display's current mode when the mirror starts; the source size is the
Mac display's pixel size (`CGDisplayCopyDisplayMode` pixel width/height). The stream is never larger than the
display. On the phone the `SurfaceView` is laid out at the stream's aspect ratio, centered in a black
`FrameLayout` that fills the display, so a later mode change of the monitor re-lays out without a restart.
Examples (tests): 3024×1964 → 1280×720 gives 1108×720; 2560×1440 gives 1280×720; 1024×768 gives 960×720;
5120×2880 → 3840×2160 gives 1920×1080. Letterboxing on the phone rather than baking bars into the stream: fewer
bits, and the phone knows its display best.

### D6. A second session for video, on the same port, with no hello change

The host opens a second TCP connection to the same address as the control session (Wi-Fi or the same `adb
forward` port, which accepts several connections) and runs the **ordinary resume handshake** with the same key.
Its first request is `MirrorAttach {stream_id}`; the agent accepts it only if the stream exists, is not attached
yet, and was started by a control session of the **same host id**. From then on that session is a media session:
no device events, no other requests, video frames in, acknowledgements and status out. Hellos, key derivation and
records stay as they are, so `PROTO_VERSION` stays 1.

Why a separate connection: on one TCP stream a 100 KB keyframe sits in front of pings, USB replies and serial
data (head-of-line blocking), and the host's 64-frame send queue would mix both. Two connections share the Wi-Fi
link but no queue. Why TCP and not QUIC datagrams (the old ARCHITECTURE plan): `adb forward` carries only TCP, one
code path for Wi-Fi and ADB, no new TLS stack beside Noise; loss on Wi-Fi shows as a stall that the pacing window
(D7) keeps from growing. QUIC stays a later option (ARCHITECTURE and ROADMAP are updated).

*Alternatives:* frames on the control session (rejected, above); a new `Intent::Media` in the hello (rejected:
changes the hello, needs a version bump); a token in the attach (unneeded: both sessions are authenticated with the
same host key, the host id check binds them).

### D7. Back-pressure: a two-frame acknowledgement window, skip at the encoder input

The phone acknowledges each frame (`VideoAck {seq}`) when its decoder took it as input. The host keeps
`in_encoder + (sent − acked) ≤ 2`. A captured frame that arrives while the window is full replaces the held
"latest" frame (the replaced one is counted as skipped); when an acknowledgement frees the window, the latest
held frame is encoded. Frames are dropped **before** encoding only, never after, because dropping an encoded P
frame breaks every following frame until the next keyframe. The window bounds the delay to about two frames plus
one round trip, also when Wi-Fi stalls or the decoder is slow. The pacing logic is a pure state machine
(`mirror::pace`) unit-tested without any I/O.

On the phone, the media session hands frames to a dedicated sink thread through a channel of 3 (window + 1). A
sink call that does not return within 2 s ends the mirror (`DecoderStalled`). The phone never drops input frames;
when the decoder reports an error, it asks for a keyframe (`VideoNeedKeyframe`) and, if the next keyframe also
fails, ends the mirror (`DecoderFailed`).

A still screen sends no frames; the host sends `Ping` on the media session after 2 s without a frame (the agent's
20 s idle timeout stays, the pong gives the media round-trip time). The agent sends `VideoStatus` every second
(decoded and dropped counts, average decode-input-to-output time, phone locked) whether frames flow or not.

### D8. Messages (appended after `DeviceClosed`)

| Message | Direction / session | Meaning |
|---|---|---|
| `GetDisplays` → `Displays(DisplayList)` | host → agent, control | list; also opts the session in to `DisplaysChanged` |
| `DisplaysChanged(DisplayList)` | agent event, control, opted-in sessions only | full list after any change |
| `MirrorStart {display_id, config: VideoConfig}` → `MirrorStarted {stream_id}` | host → agent, control | the phone shows the surface and prepares the decoder before answering |
| `MirrorAttach {stream_id}` → `Done` | host → agent, first request of the media session | |
| `MirrorStop {stream_id}` → `Done` | host → agent, control | |
| `MirrorStopped {stream_id, reason: MirrorEnd, message}` | agent event, to the owning control session only | `DisplayRemoved`, `VideoSessionLost`, `NotAttached`, `DecoderFailed`, `DecoderStalled`, `SurfaceLost`, `AgentStopped` |
| `VideoFrame(VideoFrame)` | host → agent, media, `req = 0` | `seq`, `pts_us`, `keyframe`, `config`, `data` (Annex B) |
| `VideoAck {seq}` | agent → host, media, `req = 0` | frame taken by the decoder input |
| `VideoStatus(VideoStatus)` | agent → host, media, every second | `decoded`, `dropped`, `bad`, `decode_us`, `phone_locked` |
| `VideoNeedKeyframe` | agent → host, media | |

`DisplayList {displays: Vec<DisplayInfo>, desktop_mode: bool}`; `DisplayInfo {id, name, width, height,
refresh_mhz, modes: Vec<DisplayMode>, presentation, secure}`; `VideoConfig {codec: VideoCodec::H264, width, height,
fps, bitrate_kbps}`. New `ErrorCode`s appended: `UnknownDisplay`, `DisplayFailed` (the phone cannot show a picture
there; the message says why and what to do). New error codes are sent only as answers to new requests, which only
new hosts send. Structs are final once released (postcard cannot grow them), which is why `desktop_mode`,
`phone_locked` and `bad` are in now.

Rejected start reasons: unknown display → `UnknownDisplay`; display already mirrored → `Busy`; codec, size or
fps outside what the decoder supports → `Unsupported`; the surface could not be shown (route conditions, no
decoder) → `DisplayFailed`.

### D9. Compatibility gate

- New host, old agent: an old agent's session dies on any new variant, so the host sends `GetDisplays` and the
  mirror messages only when `AgentInfo.app_version` ≥ `0.2.0` (parsed leniently; unparsable = old). The
  workspace version is bumped to `0.2.0` at the start of the implementation so dev builds pass the gate (the
  release itself stays Roman's step). Below the gate the panel says the phone app must be updated.
- Old host, new agent: the agent sends `DisplaysChanged` only to sessions that sent `GetDisplays`, and
  `MirrorStopped` only to the session that started the stream. Old hosts never send the new requests.
- Tests: an agent unit test (a session that never asked gets no display event across a hot-plug) and a host e2e
  test (an agent configured with `app_version = "0.1.0"` never receives `GetDisplays`).

### D10. Agent structure

- `Agent::set_displays(DisplayList)` (from Kotlin JSON or the simulator): diff → broadcast `DisplaysChanged`;
  every stream on a display that vanished ends with `DisplayRemoved`.
- Stream registry: `stream_id` (random u64) → display, host id, control session id, config, media session id,
  stop signal. One stream per display. A started stream not attached within 5 s ends (`NotAttached`).
- Per-session outbox: each control session registers its existing reply channel so `MirrorStopped` reaches only
  that session; `run_session` keeps a `displays_aware` flag and filters `DisplaysChanged`.
- Media mode: `run_session` switches to a media loop when the first request is `MirrorAttach`; the session's
  `SessionStatus` gets `media: true` (JSON only), so Kotlin can leave it out of the computer count.
- `VideoSink` trait (`start(stream, display, &VideoConfig) -> Result`, `frame(stream, VideoFrame) -> Result`,
  `stop(stream)`), called from blocking threads only; implementations: Android (JNI into Kotlin) and
  `fake::FakeVideoSink` (Annex B validation with `hubbridge_proto::h264`, configurable delay, counters, optional
  file). Platform → agent: `Agent::mirror_failed(stream, MirrorEnd, message)`, `Agent::request_keyframe(stream)`,
  `Agent::set_phone_locked(bool)`.
- Ending paths: host `MirrorStop` (answer `Done`, no event); display removed, media session lost, sink failure,
  stall, not attached (event to the owner); control session ended (streams of that session end silently; their
  media sessions close); agent shutdown.

### D11. Android structure

- `DisplayWatcher.kt`: `DisplayManager.getDisplays(DISPLAY_CATEGORY_PRESENTATION)` + `DisplayListener` →
  JSON `{"desktop_mode": …, "displays": [{"id", "name", "width", "height", "refresh_mhz", "modes": [{"width",
  "height", "refresh_mhz"}], "presentation", "secure"}]}` → `NativeAgent.setDisplays(json)`; a Rust test
  (`parses_display_json`) holds the field names, like `parses_scanner_json`. DeX state by Samsung's documented
  `Configuration` reflection (`SEM_DESKTOP_MODE_ENABLED` / `semDesktopModeEnabled`), false elsewhere.
- `MirrorPresentation.kt` (route S1): created on the main thread from `BridgeService`, black `FrameLayout`,
  centered `SurfaceView`, `FLAG_KEEP_SCREEN_ON`; `onMirrorStart` (JNI, blocking, from an agent thread) posts to the
  main thread and waits up to 3 s for the surface and the configured decoder; returns 0 or an error code with text.
  `onDismiss` / surface destroyed → `mirrorFailed(SurfaceLost)`.
- `MirrorDecoder.kt`: the MediaCodec of D4; `onVideoFrame(streamId, bytes, ptsUs, flags): Int` (0 queued,
  1 no input buffer in time, 2 codec error → keyframe request); output thread; counters for `VideoStatus`.
- Keyguard: `ACTION_SCREEN_OFF` / `ACTION_USER_PRESENT` + `KeyguardManager.isKeyguardLocked` → `setPhoneLocked`;
  the window then says "the phone is locked: unlock it to see the picture" (AOSP mirrors the phone under keyguard).
- Notification while mirroring: "Showing <host>'s screen on <display>"; media sessions are not counted as computers
  in the notification or in `MainActivity`.

### D12. Host and desktop structure

- `crates/proto/src/h264.rs` (pure): Annex B splitting and NAL types (SPS 7, PPS 8, IDR 5, non-IDR 1), shared by
  the fake sink and host tests.
- `crates/host/src/mirror/`: `fit.rs`, `annexb.rs` (AVCC → Annex B, parameter sets), `pace.rs`, `stats.rs`,
  `source.rs` (the `FrameSource` trait: encoded frames out, `request_keyframe`, `stop`; native size),
  `synthetic.rs` (fake Annex B at a chosen fps and size, with a "still" switch; CI and Windows),
  `test_pattern.rs` (a built-in 2 s 640×360 H.264 clip, generated once with ffmpeg and committed, with the command
  in the module doc, looped from its IDR: a real decodable stream for phone tests before the Mac capture works, and
  from Windows), `session.rs` (`MirrorSession::start(control, store, display, options, source)` →
  `MirrorHandle {stats(), stop(), done(), on_control_event()}`), `macos/{capture.rs, encode.rs, permission.rs}`
  behind `cfg(target_os = "macos")`; elsewhere the screen source returns "mirroring needs macOS for now".
- `Client::send_event(msg)` (queue a `req = 0` frame) and `Client::displays()`; the media session is a second
  `Client`, so queueing, the writer task and cancel safety are reused.
- CLI: `hubbridge displays [target] [--json]`; `hubbridge mirror [target] --display <id> [--fps 30|60]
  [--bitrate <kbit/s>] [--source screen|test-pattern|synthetic] [--to-file <path.h264> [--size WxH]]
  [--seconds N]`; `--to-file` without a target encodes the screen into a file only, fitted to `--size` (default
  1280×720): the Mac check before any phone is involved.
- Desktop: `Model.displays {supported, list, desktop_mode}`, `Model.mirror: MirrorView {state, display, size,
  fps, stats, reason, permission, phone_locked}`; requests `mirror_start {display_id, fps}`, `mirror_stop`,
  `open_screen_settings` (opens the fixed URL
  `x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture` with `/usr/bin/open`, nothing
  else); `session_loop` forwards `DisplaysChanged` and `MirrorStopped`; stats push once per second; "mirror" is a
  `begin_action` key; `stop_connection` and `disconnect_and_wait` stop the mirror first. Capture and encoder
  callbacks run on their own queues and never touch the WebView (main thread only).

### D13. Screen Recording permission

On Start: `CGPreflightScreenCaptureAccess()`; if false, `CGRequestScreenCaptureAccess()` once per app run (shows
the system prompt the first time), and the window opens a sheet: allow HubBridge in System Settings → Privacy &
Security → Screen & System Audio Recording, then quit and reopen; [Open System Settings]. The sheet also says that
an updated HubBridge must be allowed again (ad-hoc signing) and that an old entry may have to be removed with "−".
Development: a binary started from Terminal or Orca is covered by that app's permission. LIMITATIONS §5 records
all of this; Developer ID signing (roadmap) is the real fix.

### D14. The two window fixes

- Chips: `poll_adb` additionally builds `PhoneView.links: Vec<LinkView {kind: "usb" | "wifi", ready, state,
  serials, hint}>` with one entry per kind (ready if any transport of that kind is `device`); the page renders
  `links` instead of one chip per transport; the action serial prefers a ready USB transport, then a ready
  `ip:port`, then any ready one (a pure function with tests).
- Agent card: cause is in the code: `AgentView.online` comes only from the window's own mDNS browser, and the
  connection is not considered. Today the packaged app's browser did not see the phone while the terminal CLI and
  system Bonjour did, which points at macOS Local Network access of the ad-hoc signed app (LIMITATIONS §5 already
  warns that an update may ask again) — to confirm in System Settings → Privacy & Security → Local Network. Fix:
  `AgentView.link: "connected_wifi" | "connected_adb" | "on_network" | "not_seen"` computed in Rust from the
  connection and mDNS (pure function with tests); the page shows "connected · ADB", "connected · Wi-Fi", "on the
  network", "not seen on the network" (+ the Local Network hint on macOS). Pair stays offered only when mDNS sees
  the agent (pairing needs a fresh PIN on the phone anyway).

### D15. Testing without a phone

- Simulator: `--display "HDMI Screen:1280x720@60"` (repeatable), `--display-churn <s>`, `--decode-delay-ms <n>`,
  `--video-out <file.h264>`, `--lock-after <s>` (reports `phone_locked`); it uses `FakeVideoSink`.
- CI (macOS and Windows): proto round trips, pure-function tests, agent unit tests, and `crates/host/tests/
  mirror_e2e.rs` with an in-process agent, the fake sink and the synthetic source: start/attach/frames/acks/stop,
  slow sink, keyframe request, display unplug, media drop, control drop, other host's attach, old-agent gate,
  still screen with keepalive, busy display.
- macOS-only tests (`#[ignore]` by default, run with `--ignored` on a Mac): VideoToolbox encodes synthetic
  `CVPixelBuffer`s into valid Annex B with SPS/PPS/IDR; ScreenCaptureKit needs permission, so the capture is checked
  by hand with `hubbridge mirror --to-file` + `ffprobe`.
- Real phone: the `test-pattern` source checks the phone decoder independently of the Mac pipeline.

## Risks / Trade-offs

- [Samsung keeps the HDMI output on DeX even when display 8 has content] → spike route order S1–S4; NO-GO stops
  the change before mirroring code is written.
- [The phone locks or its screen goes off while mirroring: the monitor shows the lock screen or goes dark] →
  `FLAG_KEEP_SCREEN_ON` on the presentation window; `phone_locked` shown in the window; documented in LIMITATIONS §2.
  Background/screen-off mirroring is a later stage.
- [Ad-hoc signing loses Screen Recording permission on every update; macOS 15+ re-asks monthly] → clear sheet,
  LIMITATIONS §5; Developer ID signing stays on the roadmap.
- [TCP stalls on Wi-Fi spikes freeze the picture] → two-frame window keeps the delay from growing; the picture
  catches up with the newest frame; QUIC later if measurements need it.
- [VideoToolbox refuses `ConstrainedHigh` in low-latency mode] → fall back to `High_AutoLevel`; the encoder test
  logs which profile was used.
- [Exynos decoder ignores `KEY_LOW_LATENCY` or buffers frames] → measured delay in the stats; output thread renders
  newest only; checked in the hardware task.
- [CLI ScreenCaptureKit crash without a CoreGraphics session] → `CGMainDisplayID()` before any capture call.
- [Raising the minimum macOS to 13.0 drops Apple Silicon Macs still on 11–12] → negligible group; weak-link
  alternative documented in D3.
- [An old agent's session dies on a new message] → version gate (D9) with tests.
- [Phone screen kept on costs battery and heat] → only while mirroring; the phone is on a powered hub in this
  setup.

## Migration Plan

- Workspace version → 0.2.0 at the start; release notes and the tag are Roman's steps after the change.
- Phone app and desktop app can be updated in any order (D9). A debug build installs over the current debuggable
  build without losing pairing; a CI-built debug APK (other debug key) or the release APK needs an uninstall,
  which forgets paired computers.
- Rollback: the previous app versions keep working with the new ones; nothing persistent changes format.

## Open Questions

- Whether S1 also keeps the picture with the phone screen off when DeX is on (DeX may keep the HDMI group awake):
  recorded by the spike; it does not change the stage-1 design, only LIMITATIONS and the phone hint text.
- Which profile VideoToolbox accepts in low-latency mode on this Mac (D4 handles both).

## Spike results

(Filled in by the executor after task group 1: route, table of conditions, decoder name and low-latency support,
GO/NO-GO, date.)
