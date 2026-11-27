# Design

## Context

- Step 2a (archived, specs `usb-device-access`, `usb-mass-storage`) gives the host
  `Client::{open_device, claim_interface, release_interface, control, submit_transfer,
  transfer, clear_halt, close_device}`. Requests reach the agent in `submit` order; per
  endpoint the agent queues transfers in arrival order and answers each with
  `TransferDone {status, data, actual}`. `Pending::wait` gives up after `timeout +
  REQUEST_TIMEOUT (10 s)`.
- Agent facts that shape the serial path (`crates/agent/src/{lib.rs,usb.rs,nusb_backend.rs}`):
  - `timeout_ms` is capped at `MAX_TIMEOUT` = 60 s and `0` means 60 s: **there is no
    "wait forever" IN transfer.**
  - The nusb worker owns one endpoint. Its deadline is the *front* transfer's; when it
    passes, it calls `cancel_all()`, so **every** pending transfer on that endpoint comes
    back `Cancelled` (usbfs has no per-URB timeout). Other endpoints are not affected.
  - A cancelled IN completion keeps the bytes that had arrived (`completion()` truncates to
    `actual_len` whatever the status), so a `Cancelled` reply can carry data.
  - Control transfers are *not* ordered among themselves: each runs in its own task
    (`device.control_in/out(...).await`). Ordered bulk/interrupt, unordered control.
  - At most 256 pending requests per session (`MAX_IN_FLIGHT`).
  - `IDLE_TIMEOUT` = 20 s counts only frames *from the host*: a bridge whose IN transfers
    wait 60 s on an idle line would lose its session unless the host sends something.
  - The fake backend serves a waiting IN by polling every 2 ms per transfer, so two waiting
    IN transfers on one endpoint can complete out of order; it treats only interface 0 as
    claimable for bulk. Both must change for serial fakes.
- `hubbridge_proto::descriptors::parse` skips class-specific descriptors (CDC functional
  descriptors, IAD), so CDC detection walks `raw_descriptors` itself.
- Measured on this Mac (macOS 27, Python probe of `openpty`): kqueue and poll report
  readability on a pty master; `tcgetattr` on the master shows the speeds/flags an app set on
  the slave; `tcsetattr` on the slave accepts arbitrary speeds (12345, 250000, 921600,
  3000000); `IOSSIOSPEED` and `TIOCMGET` on the slave fail with `ENOTTY`; with `TIOCPKT` on
  the master, a slave `tcsetattr` produces **no** packet, a `tcflush(TCIOFLUSH)` produces
  `0x03` (FLUSHREAD | FLUSHWRITE).
- Roman's only hardware is a Chameleon Ultra, checked on the Mac on 2026-11-27: Proxgrind
  `6868:8686`, product "ChameleonUltra_ hw_v1_ fw_v515", USB 2.0 Full Speed, composite
  CDC-ACM (interface 0 class 2/2/1, interface 1 class 10). GET_APP_VERSION request
  `11ef03e8000000001500` answers `11ef03e800680002ab0203fb` (status 0x68 = success, app
  version 2.3). Direct on the Mac (`/dev/cu.usbmodem…`, 300 requests): p50 0.21 ms, p95
  0.28 ms, max 0.33 ms — the baseline for the bridged figure. UART adapters are tested with
  fakes only.

## Goals / Non-Goals

**Goals:**
- All chip logic on the desktop, unit-testable with literal request vectors taken from the
  Linux drivers; the agent stays chip-agnostic.
- PTY (macOS only) and TCP / RFC 2217 (macOS and Windows) front ends over one port core.
- CI coverage of every driver through fakes on macOS and Windows (there is no Linux platform).

**Non-Goals:**
- Windows virtual COM port (needs a signed driver such as com0com).
- XON/XOFF and DTR/DSR flow control, 9-bit/16-bit data, FTDI MPSSE/bit-bang modes,
  FT8U232AM (bcdDevice < 0x0400), CH343/CH9102 vendor mode (they enumerate as CDC-ACM and use
  that driver), Prolific PL2303.
- Two ports of one multi-port FTDI at the same time (one bridge per device; `--port`
  picks which).
- Reconnecting a bridge after the session drops (step 4: session resumption). The bridge
  stops and says why.
- Protocol or agent behaviour changes (decision 1).

## Decisions

### 1. Driver logic runs on the host; the agent and protocol stay as they are

Cost of each serial operation over Wi-Fi (RTT 2–10 ms median, 20–300 ms spikes, see
LIMITATIONS §3; one-way ≈ RTT/2), host-side driver vs. a driver in the agent:

| Operation | Host-side (this design) | Phone-side |
|---|---|---|
| Open / line change | 3–6 control requests, sequential (decision 5): 3–6 RTT ≈ 10–60 ms, on open and when the app changes settings (rare) | 1 RTT |
| RX data latency | device → phone USB (< 1 ms) + one Wi-Fi one-way; the next IN transfer is already queued (K in flight) | same |
| TX data latency | one one-way (OUT transfers pipelined, completion not awaited) | same |
| Request → reply (echo, Chameleon command) | ≈ 1 RTT + device time | same |
| Modem status, CDC-ACM | interrupt IN kept pending: one one-way per SERIAL_STATE | same |
| Modem status, CH34x | interrupt IN (Linux `ch341_read_int_callback`): one one-way | same |
| Modem status, FTDI | in-band in every IN packet: free | free |
| Modem status, CP210x | `GET_MDMSTS` polled every 200 ms, only while an RFC 2217 client is connected: 5 control round trips/s ≈ 10 small messages/s (~1 KB/s each way), change seen ≤ 200 ms + RTT | local poll, event on change |
| Idle traffic, FTDI | a 2-byte status packet per latency-timer period (16 ms) completes an IN transfer: ~62 replies + 62 resubmissions/s ≈ 12 KB/s | none |
| Sustained RX at high baud | see below — the real limit | phone buffers in RAM |

**The limit that matters is sustained RX on UART chips that send short packets.** A bulk IN
transfer completes on the first short packet. CP210x and CH34x (Linux sets CH34x divisor bit
7 so it does not wait for full 32-byte packets) and most CDC firmware answer an IN token with
whatever is buffered, so K queued transfers are used up within microseconds of data
arriving, and the endpoint then has nothing queued for about one RTT until the host's
resubmissions arrive. Meanwhile the chip buffers in its RX FIFO (CP2102: 576 B; FT232R:
256 B): 576 B lasts 50 ms at 115200 baud, 6 ms at 921600, < 2 ms at 3 Mbaud. A Wi-Fi spike
longer than that loses bytes unless RTS/CTS is wired. More transfers in flight do not help
here (they complete just as fast). FTDI is different: it holds a transfer until 62 bytes or
the latency timer, so K transfers of 16 KiB do absorb gaps. Native CDC devices (the
Chameleon, TinyUSB/ESP32-S3/RP2040 boards) hold data in firmware until an IN token comes,
so they slow down instead of losing data, unless the firmware itself drops.

A phone-side driver would fix that by resubmitting locally — but so would a *generic*
agent-side IN stream that knows nothing about chips. Chip logic itself gains nothing from
running on the phone (line control is rare, data latency is the same).

**Decision:** drivers on the host, over the step 2a transfers, no protocol change. *Why:* the
agent stays generic (no APK update per chip, nothing serial-specific on the phone), it is
the same path USB/IP and the HID proxy will use, every driver is testable on the desktop
with fakes in CI on all three OSes, and Roman's only hardware (native CDC-ACM) is not
affected by the RX-gap limit. The limit is documented (LIMITATIONS) and the IN path sits
behind one internal type (`InPump`, decision 6) so a later change can swap it.

*Alternatives:* (a) drivers in the agent (Rust, like usb-serial-for-android) — new
serial-specific messages, APK update per chip, two places to test; (b) generic agent IN
stream in this change — appended messages `StartInStream {id, endpoint, transfer_len,
depth, credit}`, events `InStreamData {id, endpoint, chunks: Vec<Vec<u8>>, status}` (URB
boundaries kept so FTDI headers still parse), `InStreamCredit`, `StopInStream`. Deferred:
it changes the agent's nusb backend and the protocol, and its benefit can only be shown on
UART hardware Roman does not have yet. Note for that change: an old agent closes the session
on an unknown `Message` variant (postcard decode error in `link`), so the host must check
`AgentInfo.app_version` before using it.

### 2. Module layout (`crates/host/src/serial/`)

| File | Contents |
|---|---|
| `mod.rs` | public types: `SerialConfig {baud, data_bits, parity, stop_bits, flow, dtr, rts}`, `Parity`, `StopBits`, `Flow`, `ModemStatus {cts, dsr, ri, dcd}`, `LineErrors`, `Stats`, `PortEvent`, re-exports |
| `detect.rs` | `detect(&UsbDeviceInfo) -> Result<Detected, NotSerial>`: driver kind, chip, interfaces, endpoints, max packet sizes, CDC capabilities |
| `cdc_acm.rs`, `cp210x.rs`, `ftdi.rs`, `ch34x.rs` | per chip: pure encoders (setup + data for each operation, divisor math, header/notification parsers) and the async open/configure/close sequences |
| `port.rs` | `SerialPort`: owns `Arc<Client>` + device id; control sequencing, IN pump, OUT writer, modem-status task, stats, events, close |
| `pty.rs` (`#[cfg(unix)]`) | PTY front end and termios watcher |
| `rfc2217.rs` | telnet / COM-PORT-OPTION codec as a pure state machine (bytes in → data + commands; encoders out) |
| `tcp.rs` | TCP listener, one client, raw or RFC 2217 |
| `bridge.rs` | `SerialBridge::start(client, &UsbDeviceInfo, BridgeOptions) -> BridgeHandle` (`info()`, `stats()`, `stop()`, `done()` with reason) |
| `bench.rs` | echo and request/response measurements over a `SerialPort` |

Drivers are an `enum Driver { CdcAcm(..), Cp210x(..), Ftdi(..), Ch34x(..) }` with `match`
dispatch — no async trait objects needed. All encoders are plain functions returning
`(Setup, Vec<u8>)` so unit tests compare literal bytes.

### 3. Reference semantics per chip (what comes from where)

- **CDC-ACM** — USB CDC PSTN 1.2 and Linux `drivers/usb/class/cdc-acm.c`: class requests
  to the control interface (`0x21`/`0xa1`, index = control interface): SET_LINE_CODING
  `0x20` (7 bytes: dwDTERate LE, bCharFormat 0/1/2 = 1/1.5/2 stop, bParityType 0–4,
  bDataBits), SET_CONTROL_LINE_STATE `0x22` (bit 0 DTR, bit 1 RTS), SEND_BREAK `0x23`
  (0xffff on, 0 off; only if ACM bmCapabilities bit 2). SERIAL_STATE notification `0x20` on
  the interrupt IN: 8-byte header + 2-byte bitmap (bit 0 DCD, 1 DSR, 2 break, 3 RI,
  4 framing, 5 parity, 6 overrun); no CTS in CDC. A STALL on SET_LINE_CODING (ACM
  capabilities bit 1 clear) is tolerated: native devices ignore line coding. Detection and
  fallbacks from cdc-acm.c (`acm_probe`: union descriptor, call-management data interface,
  "no union" pairing with the next data interface) and usb-serial-for-android
  `CdcAcmSerialDriver` (IAD-based interface pairing on composite devices).
- **CP210x** — Silicon Labs AN571 and Linux `cp210x.c`: vendor requests `0x41`/`0xc1`,
  index = interface: IFC_ENABLE `0x00`, SET_LINE_CTL `0x03` (bits 0–3 stop 0/1/2 = 1/1.5/2,
  4–7 parity 0–4, 8–15 data bits), SET_BREAK `0x05`, SET_MHS `0x07` (DTR bit 0, RTS bit 1,
  write masks bits 8/9), GET_MDMSTS `0x08` (1 byte: CTS bit 4, DSR 5, RI 6, DCD 7),
  GET_COMM_STATUS `0x10` (19 bytes, ulErrors bit 2/3 overruns, 1 framing, 4 parity), PURGE
  `0x12` (value 0x000f all; bits per direction), SET_FLOW `0x13` (16 bytes; RTS/CTS =
  CTS handshake bit 3 in ulControlHandshake + RTS mode 2 in bits 6–7 of ulFlowReplace, as
  `cp210x_set_flow_control`), SET_BAUDRATE `0x1e` (4 bytes LE), vendor `0xff` value 0x370b
  GET_PARTNUM. Open sequence and per-part maximum rates from `cp210x_open` and
  `cp210x_init_max_speed` (CP2101 921600; CP2102/CP2103 1000000; CP2104/CP2105-ECI/CP2108
  2000000; CP2102N 3000000).
- **FTDI** — Linux `ftdi_sio.c` / `ftdi_sio.h`, and usb-serial-for-android
  `FtdiSerialDriver` (header stripped per max-packet chunk of a read): vendor OUT `0x40`:
  RESET `0x00` (value 0 reset, 1 purge RX, 2 purge TX), MODEM_CTRL `0x01` (0x0101/0x0100
  DTR, 0x0202/0x0200 RTS), SET_FLOW_CTRL `0x02` (index high byte 0x01 = RTS/CTS),
  SET_BAUD_RATE `0x03`, SET_DATA `0x04` (bits 0–7 data bits, 8–10 parity 0–4, 11–13 stop
  0/1/2 = 1/1.5/2, bit 14 break), SET_LATENCY_TIMER `0x09`. Chip type by bcdDevice
  (`ftdi_determine_type`): 0x0400 BM, 0x0500 FT2232C/D, 0x0600 FT232R, 0x0700 FT2232H,
  0x0800 FT4232H, 0x0900 FT232H, 0x1000 FT-X. Divisors from `ftdi_232bm_baud_base_to_divisor`
  (base 48 MHz, `divisor3 = round(48e6 / (2·baud))`, fraction code table {0,3,2,4,1,5,6,7}
  in bits 14–16, special cases 1 → 0 and 0x4001 → 1) and `ftdi_2232h_baud_base_to_divisor`
  (base 120 MHz, `divisor3 = round(8·120e6 / (10·baud))`, same table, | 0x20000; used at
  ≥ 1200 baud on H chips). Value = divisor low 16 bits; index = divisor >> 16, and on
  FT2232C/D, FT2232H, FT4232H, FT232H `index = (divisor >> 16) << 8 | port`, port = 1 +
  bInterfaceNumber (A = 1, B = 2, …; `priv->channel = CHANNEL_A + ifnum`), so 1 on FT232H and
  FT-X too; only FT232BM and FT232R use channel 0 and a plain `divisor >> 16` (as
  `ftdi_determine_type` and `change_speed`). Other requests' index = the same port number. IN header (`ftdi_process_packet`):
  byte 0 modem (CTS bit 4, DSR 5, RI 6, DCD 7), byte 1 line (OE bit 1, PE 2, FE 3, BI 4).
  Latency timer default 16 ms (Linux default).
- **CH34x** — Linux `ch341.c` (preferred over usb-serial-for-android's older register
  script): vendor `0x40`/`0xc0`: READ_VERSION `0x5f` (2 bytes), SERIAL_INIT `0xa1`,
  WRITE_REG `0x9a`, READ_REG `0x95`, MODEM_CTRL `0xa4` (value = !(DTR 0x20 | RTS 0x40)).
  Baud: `ch341_get_divisor` (48 MHz clock, prescaler ps 0–3, fact, divisor, rounding to
  nearer, prefer fact 0 when even), written as WRITE_REG value 0x1312 (DIVISOR << 8 |
  PRESCALER), index = `(0x100 - div) << 8 | fact << 2 | ps`, | 0x80 when version > 0x27.
  LCR (WRITE_REG value 0x2518): ENABLE_RX 0x80 | ENABLE_TX 0x40 | parity (ENABLE_PAR 0x08,
  EVEN 0x10, MARK_SPACE 0x20) | STOP_BITS_2 0x04 | CS5..CS8 0..3; written only when
  version ≥ 0x30. Quirk detection (`ch341_detect_quirks`): if READ_REG of the break register
  stalls, LIMITED_PRESCALER (force fact 0 when ps < 3) and no break support. Range 46 to
  3000000 baud. Modem status from the interrupt endpoint: `!byte2 & 0x0f` (CTS 0x01, DSR
  0x02, RI 0x04, DCD 0x08; `ch341_update_status`). Break via `ch341_break_ctl` (read 0x1805,
  clear/set NBREAK 0x01 and LCR TX enable, write back); with the quirk, break is reported
  unsupported. No RTS/CTS in this change.

Literal test vectors (also in the specs): FT232R 9600 → 0x4138/0, 115200 → 0x001a/0,
921600 → 0x8003/0, 3 M → 0/0; FT232H 3 M → 0x0004/0x0201, 12 M → 0/0x0201; FT2232H port B
115200 → 0xc068/0x0202; CH341 (v0x31) 115200 → 0xcc83, 9600 → 0xb282, 12000 → 0x8386
(LIMITED_PRESCALER: 0xc182). The CH341 12000 values were derived by hand from
`ch341_get_divisor`; if the source says otherwise, the source wins and spec + tests are
corrected in the same commit.

### 4. Detection

1. VID/PID tables: CP210x `10c4` (ea60, ea61, ea63, ea70, ea71, ea7a, ea7b, …, taken from
   `cp210x.c` id_table for Silicon Labs' own ids), FTDI `0403` (6001, 6010, 6011, 6014,
   6015), CH34x `1a86` (7523, 7522, 5523). Interface: the first vendor-specific (class ff)
   interface with bulk IN + OUT (for FTDI: interface `--port`); CH34x also its interrupt IN.
2. Otherwise CDC-ACM: walk `raw_descriptors` (all descriptors with the current interface
   context). For each control interface 02/02: data interface from the union descriptor
   (`0x24`/`0x06`, bSubordinateInterface0), else from call management (`0x24`/`0x01`,
   bDataInterface), else the next interface of class 0a with one bulk IN and one bulk OUT
   (choosing the alternate setting that has them). ACM capabilities from `0x24`/`0x02`.
   Without raw descriptors (no permission) the class-based fallback still works for
   `hubbridge devices`; opening needs permission anyway.
3. Several CDC functions (e.g. dual-CDC boards): `--port N` picks the Nth; default 0.

### 5. Control requests are strictly sequential per port

The agent runs control transfers concurrently, so the port awaits each control request
before sending the next (an async mutex around "control sequences"). Open sequences are
ordered as in the Linux drivers. Before changing line settings the port waits until OUT
transfers already in flight have completed (like `TCSADRAIN`), so bytes written before a
change go out at the old settings. Control timeout 2 s; a failed control request fails the
operation with the chip's name and the request.

### 6. IN pump

- K = 4 IN transfers in flight (`--in-flight`), each `16 KiB` rounded down to a multiple of
  the endpoint's max packet size, `timeout_ms = 60000` (the agent's cap).
- Replies are awaited in submission order. For each: deliver `data` whatever the status
  (a `Cancelled` reply may carry bytes — never use `UsbReply::ok()` here); then
  `Ok`/`Cancelled` → resubmit; `Stall` → `clear_halt` then resubmit; `InvalidArgument`,
  `Fault`, `Unknown` → resubmit, and stop after 3 in a row; `Disconnected`, an
  `AgentError` `NotOpen`/`UnknownDevice`, or "connection lost" → stop the port with that
  reason. (On unplug the agent closes the device: pending transfers come back
  `Cancelled` — the nusb worker drops their reply channels — and the resubmission gets
  `NotOpen`, so the port stops within one round trip.) One reply-handling function is pure (`(status, data) -> Action`) and unit-tested.
- On an idle line all K time out together after 60 s (`cancel_all`) and are resubmitted;
  the chip buffers during that one RTT.
- FTDI: data split at max-packet boundaries, 2 header bytes stripped per chunk and fed to the
  modem/line status. Interface `InPump` hides all of this from the front ends.
- Delivery goes to a bounded channel to the front end; when the front end does not keep up,
  bytes beyond 1 MiB buffered are dropped and counted (`rx_dropped`) rather than stopping
  the pump (stopping would only move the loss into the chip's FIFO).

### 7. OUT writer

Front-end data is queued; up to 4 OUT transfers in flight, each up to 16 KiB, coalescing
whatever is queued while all slots are busy (natural batching: one message per write when
the line is idle, large transfers under load). The front end is back-pressured: it stops
reading from the PTY / TCP socket while 4 transfers and 64 KiB are pending. No zero-length
packets (no ZLP control in the protocol; cdc-acm also sends none by default).

### 8. Modem status sources

CDC-ACM and CH34x: one interrupt IN transfer (length = endpoint max packet, timeout 60 s)
kept pending and resubmitted, on its own endpoint (its timeouts never cancel bulk). FTDI:
from IN headers. CP210x: `GET_MDMSTS` every 200 ms only while some consumer has asked for
modem status (an RFC 2217 client whose modem-state mask is non-zero); PTY and raw TCP never
ask (a PTY cannot show modem lines: `TIOCMGET` fails). Changes become `PortEvent::Modem`;
line errors (CDC bits 4–6, FTDI OE/PE/FE/BI) become `PortEvent::LineError` and counters.

### 9. PTY front end (macOS only)

- `rustix` (features `pty`, `termios`, `fs`; macOS-only dependency): `openpt(RDWR | NOCTTY)`,
  `grantpt`, `unlockpt`, `ptsname`; open the slave once and **keep that fd** for the bridge's
  lifetime (the master never sees hang-up when the app closes, the app can reopen, and the
  slave termios stays readable); `make_raw()` on it.
- Master I/O through `tokio::io::unix::AsyncFd` with `O_NONBLOCK` (kqueue on the pty master
  works — measured). `TIOCPKT` on the master via `libc::ioctl` (macOS-only `libc`
  dependency): data packets start with `TIOCPKT_DATA` (0, stripped); `FLUSHREAD` /
  `FLUSHWRITE` become purges of RX / TX.
- Termios watcher: `tcgetattr` on the held slave fd every 100 ms **and** before forwarding
  each chunk read from the master; a change of speed, CSIZE, PARENB/PARODD, CSTOPB or
  CRTSCTS becomes a line change applied (decision 5) before that chunk goes out. Speeds are
  read as numbers (macOS `speed_t` is the rate itself), so 250000 works. `--lock-line` disables the watcher.
- Facts for users (LIMITATIONS): apps that set rates above 230400 through `IOSSIOSPEED`
  (pyserial on macOS) get `ENOTTY` on a PTY — use `--baud N --lock-line`, or RFC 2217; DTR/RTS
  toggling (esptool auto-reset) is impossible on a PTY — use RFC 2217
  (`rfc2217://127.0.0.1:PORT`). Bytes that arrive while no app has the PTY open wait in the
  pty queue and are read by the next app that opens it.

### 10. TCP and RFC 2217 front ends

- `tokio::net::TcpListener`, default 127.0.0.1; a bare port (`--tcp 7000`) means
  127.0.0.1:7000; a non-loopback address prints a warning (no authentication). One client;
  extra connections are accepted and closed at once. `TCP_NODELAY` on.
- Raw mode: bytes as is.
- RFC 2217 (RFC 854/855 telnet, RFC 856 BINARY, RFC 858 SGA, RFC 2217): on connect the
  server sends `IAC WILL BINARY, IAC DO BINARY, IAC WILL SGA, IAC WILL COM-PORT-OPTION`;
  answers DO/WILL for those and DONT/WONT for anything else, replying only on a state change
  (no negotiation loops). 0xff doubled in both directions, also inside SB values. SB parsed
  across TCP read boundaries. Commands: SIGNATURE (0) → "HubBridge <device label>";
  SET-BAUDRATE (1, 4 bytes BE, 0 = query); SET-DATASIZE (2); SET-PARITY (3: 1 none, 2 odd,
  3 even, 4 mark, 5 space); SET-STOPSIZE (4: 1 = 1, 2 = 2, 3 = 1.5); SET-CONTROL (5: 0/1/3
  flow query/none/hardware, 2 XON/XOFF → reply with the flow in effect, 4/5/6 break
  query/on/off, 7/8/9 DTR, 10/11/12 RTS, 13–19 inbound flow → reply with current);
  FLOWCONTROL-SUSPEND/RESUME (8/9: stop/resume sending data to the client);
  SET-LINESTATE-MASK (10, default 0), SET-MODEMSTATE-MASK (11, default 255); PURGE-DATA
  (12: 1 RX, 2 TX, 3 both). Replies use code + 100 and carry the value in effect, sent after
  the port finished the change. NOTIFY-MODEMSTATE (107): CTS 0x10, DSR 0x20, RI 0x40,
  DCD 0x80, deltas 0x01/0x02/0x08, RI trailing edge 0x04, masked; NOTIFY-LINESTATE (106):
  overrun 0x02, parity 0x04, framing 0x08, break 0x10, masked. `--lock-line` answers sets
  with the current values without changing them.
- Interop: pyserial `rfc2217://` and esptool (`--port rfc2217://127.0.0.1:PORT`) are the
  reference clients; the e2e tests use a small Rust client in the test file.

### 11. Bridge lifecycle and keepalive

`SerialBridge` owns a `SerialPort` and its front ends; it ends on `stop()`, on a port
failure (device closed, unplugged, session lost) or a front-end failure, always closing the
port (DTR/RTS off, CP210x IFC_ENABLE 0, release, close). The library does not ping: the
desktop already pings every second; the CLI `serial` command pings every 2 s (agent idle
timeout 20 s). The library does not take the client's event stream (the desktop owns it);
device loss is seen through transfer status / `NotOpen`, and the desktop additionally stops
bridges on `DeviceClosed` / `DeviceRemoved`.

### 12. Fakes and fixtures

- `crates/proto/src/fixtures.rs`: raw descriptors for FT232R (0403:6001, bcdDevice 0x0600,
  EP 0x81/0x02, 64 B), FT2232H (0403:6010, 0x0700, interfaces 0 and 1, EP 0x81/0x02 and
  0x83/0x04, 512 B), FT232H (0403:6014, 0x0900, 512 B), CH340 (1a86:7523, interface
  ff/01/02, bulk 0x82/0x02 32 B, interrupt 0x81 8 B), CDC-ACM simple (1209:0001: IAD, control
  0 with header/call-management/ACM caps 0x06/union, interrupt IN 0x83, data 1 with bulk
  0x81/0x02 64 B), CDC-ACM composite (two vendor interfaces first, IAD at 2, control 2, data
  3), CDC-ACM without union (control 0 without functional descriptors, data 1); plus
  `UsbDeviceInfo` builders like `cp2102_uart`.
- `crates/agent/src/fake.rs` gains `FakeKind::Serial(SerialChip)` with the emulation in a new
  `crates/agent/src/fake/serial.rs`; `SerialChip` = `Cp210x {partnum}`, `Ftdi`, `Ch34x
  {version, limited_prescaler}`, `CdcAcm`. Each answers its chip's control requests (stall
  on unknown ones), records every control request and every OUT write in one ordered event
  log plus the decoded state (baud, format, flow, DTR, RTS, break, purges, enabled, latency
  timer), and loops OUT data back to IN (CP210x only after IFC_ENABLE 1, like the chip).
  FTDI wraps IN data in packets with the 2-byte header and returns a status-only packet
  after the latency timer when idle. CDC-ACM and CH34x queue interrupt packets on modem
  changes. Test hooks on `FakeUsb`: `serial_log(id)`, `set_modem(id, ModemLines)`,
  `inject_rx(id, bytes)`, `inject_line_error(id, …)`.
- Generic fake fixes: endpoint → interface map from the raw descriptors (claim check per
  owning interface); waiting IN transfers per endpoint served strictly in submission order,
  and when the front one times out all waiting ones on that endpoint return `Cancelled`
  (as the nusb worker's `cancel_all`).
- Simulator: 1002 becomes the CP210x fake (still loops back), new 1005 FT232R, 1006 FT2232H,
  1007 FT232H, 1008 CH340, 1009 CDC-ACM; 1003 keyboard and 1004 drive unchanged.

### 13. Measurement modes

Generic, in `serial/bench.rs`, driven by the CLI; nothing device-specific in the library.
- `--bench-echo [--count 200] [--size 32] [--bytes 1048576] [--window 512]`: latency =
  write `size` patterned bytes, wait for all of them back (p50/p95/max over `count`);
  throughput = stream `bytes` with at most `window` bytes unacknowledged (keeps the chip
  FIFO from overflowing), verify every byte, report KB/s, % of the line rate
  (baud / bits-per-char) and lost/wrong bytes.
- `--bench-request <hex> (--reply-len N | --reply-idle-ms M) [--count 100]
  [--interval-ms 0]`: per round, purge host RX buffer, write the bytes, collect until N bytes
  or M ms idle after the first byte (timeout `--reply-timeout-ms`, default 1000, counts as
  timed out); print the first reply in hex, latency percentiles, timeouts and reply
  throughput (reply bytes / time).

### 14. Desktop window

`Model` gains `serial_capable: Vec<SerialCapable {id, driver, chip}>` (from `detect` when
devices change), `serial: Vec<SerialView {id, driver, chip, pty, tcp, rfc2217, line, tx,
rx, dropped, errors, running, error}>` and `pty_supported` (`cfg!(unix)`). `Request` gains
`SerialStart {id, baud, pty, tcp_port, rfc2217}` and `SerialStop {id}`. The controller keeps
`HashMap<u32, BridgeHandle>`; the existing 1 s ping tick refreshes counters while bridges
run; bridges stop on Disconnect, reconnect (new client), `DeviceClosed` and `DeviceRemoved`.
The device card gets a "Serial" row: baud select (9600 … 3000000 and custom), PTY toggle
(hidden on Windows), TCP port field with RFC 2217 switch, Start/Stop, path with a copy
button, counters. en/ru strings.

## Risks / Trade-offs

- [UART chips lose RX bytes at high baud during Wi-Fi stalls (decision 1)] → documented
  with numbers in LIMITATIONS; `--flow rtscts` where wired; overruns visible in counters
  (FTDI/CDC flags, CP210x GET_COMM_STATUS at bench end); generic agent IN stream sketched
  as the follow-up, to decide after the optional UART hardware run.
- [Every IN on an endpoint is cancelled when the front one times out] → only on a line
  idle for 60 s; resubmitted within one RTT; cancelled replies' data is kept.
- [Host session idle timeout with long IN transfers] → CLI pings every 2 s; desktop already
  pings.
- [Control transfers unordered on the agent] → strictly sequential per port.
- [FTDI idle status packets cost ~62 small messages/s each way] → acceptable on a LAN;
  latency timer configurable (`--ftdi-latency-ms`, 1–255).
- [PTY: no modem lines, `IOSSIOSPEED` fails, stale bytes after reopen] → documented;
  RFC 2217 for apps that need line control.
- [Unauthenticated local TCP port] → 127.0.0.1 by default, warning otherwise.
- [Fakes are written from the same reading of the drivers as the host code] → literal
  vectors from the Linux sources in unit tests and specs; Chameleon run on real hardware;
  UART adapters on hardware as soon as Roman has one (optional task).
- [Chameleon specifics] → VID/PID, frame format and GET_APP_VERSION verified on the Mac; a
  cheap bulk-data command for throughput is still to be found in the ChameleonUltra repo;
  only read-only commands.

## Migration Plan

Host-only feature: no protocol or agent change, works with the APK already on the phone
(step 2a). The APK is rebuilt anyway for the hardware check (the fake backend compiles into
the Android library). Rollback: an older host simply lacks `hubbridge serial`.

## Open Questions

- Which UART adapter (if any) Roman will get decides when the optional hardware task runs
  and whether the generic agent IN stream follow-up is needed.
- Exact Chameleon Ultra read-only command for a throughput figure (if none returns a lot of
  data, throughput is skipped and the report says so).
