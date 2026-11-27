# Spec Delta

## Purpose

Makes a USB serial port on the phone usable by ordinary desktop applications: as a
pseudo-terminal on macOS, as a raw or RFC 2217 TCP port on macOS and Windows, from the
`hubbridge serial` command and from the desktop window, with built-in latency and
throughput measurements.

## ADDED Requirements

### Requirement: Pseudo-terminal front end
On macOS the bridge SHALL be able to expose the port as a pseudo-terminal in raw
mode and print its device path; bytes written to it SHALL go to the device and bytes from
the device SHALL be readable from it. An application closing and reopening it SHALL not
stop the bridge.

#### Scenario: Echo through the PTY
- **WHEN** an application opens the printed `/dev/ttys…` path of a bridge on a loopback device and writes "hello"
- **THEN** it reads "hello" back

#### Scenario: Reopen
- **WHEN** the application closes the PTY and opens it again
- **THEN** the bridge is still running and data flows again

#### Scenario: Not on Windows
- **WHEN** the user asks for a PTY on Windows
- **THEN** the command fails saying to use `--tcp` (with `--rfc2217` for line control)

### Requirement: PTY line settings reach the chip
The bridge SHALL apply the baud rate, data bits, parity, stop bits and RTS/CTS flag an
application sets on the PTY to the device, before forwarding any data the application
writes after the change, and SHALL turn a flush of the PTY into a purge of the matching
direction. With `--lock-line` it SHALL ignore those settings.

#### Scenario: Application changes the baud rate
- **WHEN** an application sets 57600 baud, even parity on the PTY and then writes data
- **THEN** the device receives the new line settings before that data

#### Scenario: Non-standard rate on macOS
- **WHEN** an application on macOS sets 250000 baud on the PTY through tcsetattr
- **THEN** the device is set to 250000 baud

#### Scenario: Locked line
- **WHEN** the bridge runs with `--baud 921600 --lock-line` and the application sets 9600 baud
- **THEN** the device stays at 921600 baud

### Requirement: Raw TCP front end
The bridge SHALL be able to listen on a TCP address, 127.0.0.1 unless the user names
another, and pass bytes unchanged between one connected client and the device; a second
client SHALL be refused while one is connected.

#### Scenario: Raw client
- **WHEN** a TCP client connects to a raw bridge on a loopback device and sends bytes 00..ff including 0xff
- **THEN** it receives the same 256 bytes back unchanged

#### Scenario: Second client
- **WHEN** a second client connects while one is connected
- **THEN** the second connection is closed and the first keeps working

#### Scenario: Default bind address
- **WHEN** the user gives `--tcp 7000` without an address
- **THEN** the bridge listens on 127.0.0.1:7000 only

### Requirement: RFC 2217 front end
With `--rfc2217` the TCP front end SHALL speak telnet with the COM-PORT-OPTION (44):
escape 0xff in both directions, answer option negotiation, and apply SET-BAUDRATE,
SET-DATASIZE, SET-PARITY, SET-STOPSIZE, SET-CONTROL and PURGE-DATA to the port, replying
with the value in effect (code + 100) after the device took it.

#### Scenario: Set baud rate
- **WHEN** an RFC 2217 client sends IAC SB COM-PORT-OPTION SET-BAUDRATE, i.e. bytes ff fa 2c 01 00 01 c2 00 ff f0
- **THEN** the device is set to 115200 baud and the client receives ff fa 2c 65 00 01 c2 00 ff f0

#### Scenario: Query without change
- **WHEN** a client sends SET-DATASIZE with value 0
- **THEN** the reply carries the current data size and the device gets no request

#### Scenario: Escaped data
- **WHEN** the device sends byte 0xff to an RFC 2217 client
- **THEN** the client receives ff ff, and a client's ff ff reaches the device as one 0xff

#### Scenario: DTR, RTS and break
- **WHEN** a client sends SET-CONTROL 9 (DTR off), 11 (RTS on) and 5 (break on)
- **THEN** the port drops DTR, raises RTS and starts a break, and each is acknowledged with code 105

#### Scenario: Unsupported flow control
- **WHEN** a client asks for XON/XOFF flow control (SET-CONTROL 2)
- **THEN** the reply carries the flow control in effect (1, none or 3, hardware)

### Requirement: RFC 2217 notifications
The RFC 2217 front end SHALL send NOTIFY-MODEMSTATE when a modem line covered by the
client's modem-state mask changes (default mask 255) and NOTIFY-LINESTATE for line errors
covered by its line-state mask (default 0), and SHALL stop and resume sending data on
FLOWCONTROL-SUSPEND and FLOWCONTROL-RESUME.

#### Scenario: Modem line change
- **WHEN** DCD turns on at the device while an RFC 2217 client is connected with the default mask
- **THEN** the client receives NOTIFY-MODEMSTATE (code 107) with the DCD and delta-DCD bits set

#### Scenario: Line state masked out
- **WHEN** the device reports a framing error and the client has not set a line-state mask
- **THEN** no NOTIFY-LINESTATE is sent

### Requirement: The `hubbridge serial` command
`hubbridge serial <id> [target]` SHALL open the device as a serial port with the line
options given (`--baud`, `--data-bits`, `--parity`, `--stop-bits`, `--flow`, `--dtr`,
`--rts`, `--port` for multi-port FTDI), start the chosen front ends (`--pty`, `--tcp`,
`--rfc2217`; PTY by default on macOS), print where they are, and run until
Ctrl-C or until the device or session goes away.

#### Scenario: Start with a PTY
- **WHEN** the user runs `hubbridge serial 1002 --baud 115200` on macOS against the simulator
- **THEN** it prints the driver (CP210x), the line (115200 8N1) and the PTY path, and data written to that PTY comes back

#### Scenario: Ctrl-C
- **WHEN** the user presses Ctrl-C
- **THEN** the port is closed on the agent, the PTY disappears and the command exits with status 0

#### Scenario: Session kept alive
- **WHEN** the bridge runs for a minute with no data on the line
- **THEN** the agent does not close the session for idleness

#### Scenario: Device unplugged
- **WHEN** the device is unplugged from the phone while the command runs
- **THEN** the command exits with an error saying the device was closed

### Requirement: Echo measurement
`hubbridge serial <id> --bench-echo` SHALL measure, on a device whose TX is wired to its
RX, the round-trip latency of small writes (count, p50, p95, max) and the throughput of a
long patterned stream, checking every byte that comes back and reporting lost or wrong
bytes.

#### Scenario: Echo benchmark on the simulator
- **WHEN** the user runs `hubbridge serial 1002 --bench-echo` against the simulator
- **THEN** it prints the latency percentiles, the throughput in KB/s with the share of the line rate, and "0 bytes lost"

### Requirement: Request/response measurement
`hubbridge serial <id> --bench-request <hex>` SHALL send the given bytes repeatedly
(`--count`, `--interval-ms`), wait for each reply until `--reply-len` bytes or until the
line is idle for `--reply-idle-ms`, and print the reply of the first round in hex, the
latency percentiles and the reply throughput.

#### Scenario: Fixed-length reply
- **WHEN** the user runs the request mode with `--reply-len 12 --count 200`
- **THEN** it prints the first 12-byte reply in hex and the p50/p95/max request-to-reply latency of 200 rounds

#### Scenario: No reply
- **WHEN** the device does not answer within `--reply-idle-ms` of a request
- **THEN** that round is counted as timed out and the command goes on with the next round

### Requirement: Serial driver in the device list
`hubbridge devices` SHALL show, for each device a serial driver fits, which driver and
chip it would use.

#### Scenario: Simulator device list
- **WHEN** the user runs `hubbridge devices` against the simulator
- **THEN** the CP2102 device shows "serial: CP210x" and the flash drive shows no serial line

### Requirement: Serial bridge in the desktop window
The desktop window SHALL offer, on each serial-capable device with permission, a serial
bridge that the user can start with a baud rate and a PTY (macOS) and/or TCP port (with an
RFC 2217 switch), and SHALL show while it runs the PTY path or TCP address, driver, line
settings and byte counters, with a Stop button.

#### Scenario: Start from the window
- **WHEN** the user starts a bridge with a PTY on the simulator's CP2102 in the window on macOS
- **THEN** the device card shows the PTY path, "CP210x · 115200 8N1" and counters that grow as data flows

#### Scenario: Phone disconnects
- **WHEN** the connection to the phone drops while a bridge runs
- **THEN** the bridge stops and the card says why

#### Scenario: Windows
- **WHEN** the window runs on Windows
- **THEN** only the TCP front end is offered
