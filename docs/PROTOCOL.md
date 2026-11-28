# HubBridge protocol v1

Code: `crates/proto` (types), `crates/link` (records, pairing, encryption).

## Transport

One TCP connection per session. Default agent port **47800**. The same port serves
Wi-Fi/LAN clients and `adb forward tcp:<local> tcp:47800` clients.

Discovery: DNS-SD service `_hubbridge._tcp`, TXT records:

| key | value |
|---|---|
| `id` | agent id (32 hex chars) |
| `v` | protocol version (`1`) |
| `name` | agent name (phone's device name) |

## Records

Every record is `u16` big-endian length + body (≤ 65535 bytes).

## Connection setup

```
host                                              agent
 │ ClientHello {magic "HBRG", version, host_id,     │
 │   host_name, intent: Pair{spake_msg} | Resume}  ─▶│  plaintext record
 │◀─ ServerHello {version, agent_id, agent_name,    │  plaintext record
 │     result: Proceed{spake_msg?} | Rejected{..}}  │
 │ Noise NNpsk0 msg 1 (psk, e)                     ─▶│
 │◀─ Noise NNpsk0 msg 2 (e, ee)                      │
 │════════ encrypted records (Frames) ══════════════│
```

- Hellos are postcard-encoded structs (`ClientHello`, `ServerHello` in `crates/proto`).
- `transcript = SHA-256("hubbridge transcript v1" ‖ len ‖ ClientHello bytes ‖ len ‖ ServerHello bytes)`
  is the Noise **prologue**: changing either hello in flight breaks the handshake.
- **Pairing** (`Intent::Pair`): SPAKE2 over Ed25519 with the 6-digit PIN as password,
  identities `hubbridge-host` / `hubbridge-agent`. From the SPAKE2 secret `K`:
  - `psk = HKDF-SHA256(K, salt = transcript, info = "hubbridge pairing psk")`
  - `long_term_key = HKDF-SHA256(K, salt = transcript, info = "hubbridge long-term key")`

  A wrong PIN gives different keys; the agent fails to decrypt Noise message 1 and closes.
  The agent opens a pairing window only on demand (Pair button), for 120 s, single use,
  3 attempts, then the PIN is burned.
- **Resume** (`Intent::Resume`): the agent looks up `host_id`; unknown → `Rejected{NotPaired}`.
  The host picks the key by the `agent_id` in `ServerHello` (so it need not know in advance
  which phone answers on an IP or an adb port).
  `psk = HKDF-SHA256(long_term_key, salt = transcript, info = "hubbridge resume psk")`.
- Noise: `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`, empty payloads, then stateless transport
  mode with a 64-bit nonce counter per direction, starting at 0.

Rejection codes: `BadMagic`, `VersionMismatch`, `NotPaired`, `PairingNotActive`,
`PairingLocked`, `Busy`.

## Encrypted messages

An application message is `u32` big-endian length + postcard bytes of a `Frame`, split into
plaintext chunks of ≤ 65519 bytes, each chunk one Noise ciphertext record. Max message
16 MiB.

```rust
struct Frame { req: u32, msg: Message }   // req = 0: unsolicited event
```

The requester picks a non-zero `req`; the response carries the same `req`.

| Message | Direction | Meaning |
|---|---|---|
| `Ping {seq, sent_us}` / `Pong {seq, sent_us}` | either → echo | liveness, RTT |
| `GetAgentInfo` → `AgentInfo {agent_id, name, model, os, app_version, proto_version, usb_host}` | host → agent | |
| `ListDevices` → `DeviceList {devices}` | host → agent | full list; also sent as an event after the host fell behind |
| `DeviceAdded(UsbDeviceInfo)` | agent event | new or changed device (e.g. permission granted) |
| `DeviceRemoved {id}` | agent event | unplugged |
| `RequestPermission {id}` → `PermissionRequested {id}` | host → agent | phone shows the USB permission dialog; outcome arrives as `DeviceAdded` |
| `Error {code, message}` | response | `UnknownDevice`, `Unsupported`, `Internal` |
| `Bye` | either | orderly close |

`UsbDeviceInfo`: `id`, `path`, `vendor_id`, `product_id`, `class/subclass/protocol`,
`bcd_usb?`, `bcd_device?`, `manufacturer?`, `product?`, `serial?`, `configurations[]`
(→ interfaces with alternates → endpoints), `has_permission`, `raw_descriptors?` (device
descriptor + all configuration descriptors, as usbfs returns them).

Liveness: the agent closes a session after 20 s without any message; hosts ping every 2 s
in `watch`.

## Evolution

- New `Message` variants are only appended (postcard encodes the variant index).
- Incompatible changes bump `PROTO_VERSION`; the agent answers `VersionMismatch`.
- The displays and mirroring messages (step 3a, below) are appended variants: hellos, key
  derivation and records are unchanged, so the protocol stays **v1**.
- A peer that receives a variant it does not know fails to decode the frame and **its session
  ends**. New messages are therefore sent only to peers that are known to understand them
  (see "Compatibility").

## USB data (step 2)

| Request (host → agent) | Reply | Notes |
|---|---|---|
| `OpenDevice {id}` | `Done` | exclusive: `Busy` if another session has it; `NoPermission` until the user allows USB access on the phone |
| `CloseDevice {id}` | `Done` | also automatic when the session ends |
| `ClaimInterface {id, interface, detach_kernel_driver}` | `Done` | detaching takes the interface from the phone's own driver (a flash drive is unmounted, a keyboard stops typing on the phone) |
| `ReleaseInterface {id, interface}`, `SetAlternate {id, interface, alternate}` | `Done` | |
| `Control {id, setup, data, timeout_ms}` | `ControlDone {status, data}` | direction from `setup.request_type` bit 7 |
| `Transfer {id, endpoint, length, data, timeout_ms}` | `TransferDone {status, data, actual}` | bulk/interrupt; IN reads up to `length`, OUT sends `data`; ≤ 1 MiB |
| `ClearHalt {id, endpoint}`, `ResetDevice {id}` | `Done` | |

- `status`: `Ok`, `Stall`, `Cancelled` (also timeout), `Disconnected`, `Fault`,
  `InvalidArgument`, `Unknown` — a transfer that ran but failed is a `*Done` with that
  status; a request that could not run is an `Error {code}` (`NotOpen`, `UsbFailed`...).
- Ordering: requests are queued in the order they arrive, so a host may keep many
  transfers in flight on one endpoint (the mass-storage client keeps a whole `READ(10)` data
  phase in flight) and they complete in order. The agent stops reading new requests while
  256 are pending in a session.
- Event `DeviceClosed {id, reason}` when the agent closes an open device (unplugged).

## Displays and mirroring (step 3a)

The host shows a video stream on a display attached to the phone. Types are in
`crates/proto/src/video.rs`; these structs are final once released.

| Message | Direction, session | Meaning |
|---|---|---|
| `GetDisplays` → `Displays(DisplayList)` | host → agent, control | the list; also opts this session in to `DisplaysChanged` |
| `DisplaysChanged(DisplayList)` | agent event, control, opted-in sessions only | the full list after any change |
| `MirrorStart {display_id, config: VideoConfig}` → `MirrorStarted {stream_id}` | host → agent, control | the phone shows the surface and prepares the decoder before answering (at most 5 s) |
| `MirrorAttach {stream_id}` → `Done` | host → agent, first request of the media session | |
| `MirrorStop {stream_id}` → `Done` | host → agent, control | the phone removes the picture and closes the media session |
| `MirrorStopped {stream_id, reason, message}` | agent event, only to the control session that started the stream | the phone ended it; `reason`: `DisplayRemoved`, `VideoSessionLost`, `NotAttached`, `DecoderFailed`, `DecoderStalled`, `SurfaceLost`, `AgentStopped` |
| `VideoFrame {seq, pts_us, keyframe, config, data}` | host → agent, media, `req = 0` | one Annex B access unit; `config` = SPS and PPS only |
| `VideoAck {seq}` | agent → host, media, `req = 0` | the decoder input took the frame |
| `VideoStatus {decoded, dropped, bad, decode_us, phone_locked}` | agent → host, media, `req = 0`, every second | counters since the stream started |
| `VideoNeedKeyframe` | agent → host, media, `req = 0` | send a keyframe |

- `DisplayList {displays, desktop_mode}`; `desktop_mode` is Samsung DeX.
  `DisplayInfo {id, name, width, height, refresh_mhz, modes[{width, height, refresh_mhz}],
  presentation, secure}`. A display id is valid only until the list changes (a replug or DeX
  can bring the same monitor back under another id).
- `VideoConfig {codec: H264, width, height, fps, bitrate_kbps}`.
- `Error` codes of a refused start: `UnknownDisplay` (no such display), `Busy` (it already
  shows a mirror), `Unsupported` (no video output, another codec, zero size, fps outside
  1–60), `DisplayFailed` (the phone cannot show a picture there now; the message says why and
  what to do, e.g. turn Samsung DeX off). New codes answer new requests only.

### The media session

Video does not share a connection with control traffic. The host opens a **second TCP
connection** to the same address (Wi-Fi, or the same `adb forward` port) and runs the
ordinary **resume** handshake with the same key. Its first request is `MirrorAttach`. The
agent accepts it only if the stream exists, is not attached yet and was started by a control
session of the **same host id**. From then on this is a media session: no device or display
events; the only messages are `VideoFrame`, `VideoAck`, `VideoStatus`, `VideoNeedKeyframe`,
`Ping`/`Pong` and `Bye`. Any other message ends it. `MirrorAttach` on a session that already
sent another request is answered `Unsupported`.

- **Acknowledgement window:** the host keeps at most two frames in its encoder or sent and not
  yet acknowledged, and skips captured frames (before encoding) while the window is full.
  The agent passes frames to the decoder one at a time and never drops one; a decoder that
  takes nothing for 2 s ends the mirror (`DecoderStalled`).
- **Keyframes:** the stream starts with a config frame and a keyframe; the host sends a new
  keyframe when asked and at least every 10 s, at most two forced ones per second.
- **Still screen:** no frames are sent. The host sends `Ping` every 2 s, with or without
  frames, so the agent's 20 s idle timeout never ends a quiet stream; the `Pong` gives the
  media RTT.
- A stream that is not attached within 5 s ends (`NotAttached`). When the control session
  ends, its streams end without an event.

### Compatibility

- A host sends `GetDisplays` and the mirror messages only when `AgentInfo.app_version` is at
  least **0.2.0** (parsed leniently: leading digits of each part; unparsable counts as old).
- The agent sends `DisplaysChanged` only to sessions that sent `GetDisplays`, and
  `MirrorStopped` only to the session that started the stream, so an older host receives
  nothing new.

