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
- Step 3 adds `OpenVideo`/`VideoConfig` on the control stream and a separate media
  connection.

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
