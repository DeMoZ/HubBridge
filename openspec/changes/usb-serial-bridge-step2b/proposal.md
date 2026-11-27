# Proposal

## Why

Step 2a gives the desktop host generic USB transfers to devices on the phone's hub, but
nothing on the desktop uses them yet except the mass-storage test client. USB-UART adapters
and boards with a native USB serial port (MCU boards, the Chameleon Ultra RFID tool) are the
first useful thing to bridge: they need no OS driver on the desktop, macOS can show them as a
pseudo-terminal, and any OS can reach them over TCP / RFC 2217 (Windows has no driver-free
virtual COM port). This is ROADMAP step 2, item 3.

## What Changes

- **Serial drivers on the host** (`crates/host/src/serial/`), built only on the step 2a
  client API (`open_device`, `claim_interface`, `control`, `submit_transfer`, `clear_halt`):
  CDC-ACM (class descriptors), Silicon Labs CP210x, FTDI (FT232R / FT-X / FT2232C/D /
  FT2232H / FT4232H / FT232H), WCH CH340/CH341. Line settings (baud, data bits, parity,
  stop bits, RTS/CTS), DTR/RTS, break, purge, modem status (CDC SERIAL_STATE and CH34x
  interrupt endpoint, FTDI in-band status header, CP210x control polling).
- **Driver detection** from the device's descriptors: VID/PID tables for the vendor chips,
  CDC-ACM control + data interface pairs from interface association, union and call
  management functional descriptors, with the Linux `cdc-acm` fallbacks.
- **The driver logic runs on the desktop, not on the phone.** The agent and the protocol do
  not change: no new messages, no APK update for a new chip. Throughput limits this implies
  for UART chips at high baud rates over Wi-Fi are documented, and a generic agent-side IN
  stream is sketched as a possible follow-up (design.md, decision 1).
- **Front ends on the desktop:**
  - a pseudo-terminal on macOS (and Linux, for the CLI) — `/dev/ttysNNN`; settings the
    application makes on it (baud, parity, data and stop bits, RTS/CTS) are applied to the
    chip; `tcflush` purges;
  - a TCP port, raw or RFC 2217 (telnet COM-PORT-OPTION: baud, data size, parity, stop size,
    DTR/RTS/flow/break control, modem/line state notifications, purge), bound to 127.0.0.1
    by default; works on Windows without a driver.
- **CLI** `hubbridge serial <id> [target]` with line options, `--pty`, `--tcp <addr>`,
  `--rfc2217`, and generic measurement modes: `--bench-echo` (TX-RX loopback hardware) and
  `--bench-request <hex>` (request/response latency, optional reply-throughput).
  `hubbridge devices` names the serial driver of each serial-capable device.
- **Desktop window:** start/stop a serial bridge for a serial-capable device, show the PTY
  path / TCP address, driver, line settings and byte counters.
- **Tests without hardware:** fake CP210x, FTDI (FT232R, FT2232H, FT232H), CH340 and CDC-ACM
  devices in the agent's fake backend that record the requests they get and loop TX back to
  RX; descriptor fixtures for each; e2e tests through the PTY (unix) and RFC 2217 (all
  platforms) in CI. The fake backend also starts completing waiting IN transfers in
  submission order, as the real backend does.
- **Hardware check:** CDC-ACM with Roman's Chameleon Ultra over Wi-Fi (request/response
  latency with a read-only command); UART adapters are fake-tested only until hardware is
  available (optional task).
- Docs: ARCHITECTURE (OS integration layer), LIMITATIONS (serial row and limits), ROADMAP.
  PROTOCOL.md unchanged.

## Capabilities

### New Capabilities
- `usb-serial`: driving a USB serial device on the phone from the desktop host — detecting
  which driver fits, configuring the line, moving data both ways, modem and line status, and
  giving the device back on close.
- `serial-bridge`: exposing such a port to desktop applications — pseudo-terminal, raw TCP
  and RFC 2217 front ends, the `hubbridge serial` command with its measurement modes, and
  the desktop window's serial controls.

### Modified Capabilities
<!-- none: usb-device-access and usb-mass-storage keep their requirements. The fake
     backend's IN ordering fix brings the test backend in line with the existing
     "complete in request order" requirement; it does not change it. -->

## Impact

- Code: new `crates/host/src/serial/` (detect, cdc_acm, cp210x, ftdi, ch34x, port, pty,
  rfc2217, tcp, bridge, bench), `crates/host/src/main.rs` (`serial` command),
  `crates/host/src/format.rs` (serial driver line), `crates/desktop` (Model, Request, page),
  `crates/agent/src/fake.rs` + new `crates/agent/src/fake/serial.rs` (chip emulation),
  `crates/proto/src/fixtures.rs` (descriptor fixtures), `crates/agent-sim` (registers the
  fakes), new tests `crates/host/tests/serial_e2e.rs`.
- Agent behaviour on the phone: unchanged (the fake backend is compiled into the Android
  library but not used there); the APK is rebuilt only for the real-phone check.
- Dependencies: `rustix` (pty, termios; unix only) in `hubbridge-host`.
- Protocol: unchanged (v1, no new messages).
- Docs: `docs/ARCHITECTURE.md`, `docs/LIMITATIONS.md`, `docs/ROADMAP.md`.
