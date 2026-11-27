# Spec Delta

## Purpose

Lets the desktop host drive a USB serial device attached to the phone — CDC-ACM, CP210x,
FTDI or CH34x — through the generic USB transfers: pick the driver, configure the line,
move data both ways, report modem and line status, and give the device back when done.

## ADDED Requirements

### Requirement: Serial driver detection
The host SHALL pick a serial driver from the device's descriptors: the vendor chips by
VID/PID (CP210x 10c4, FTDI 0403, CH34x 1a86 with their known product ids) and any device
with a CDC-ACM control interface (class 02, subclass 02) paired with a CDC data interface
(class 0a). A device that matches neither SHALL be refused with a message naming the
supported kinds, before it is opened.

#### Scenario: Vendor chip
- **WHEN** the host checks a device 10c4:ea60
- **THEN** it picks the CP210x driver with the device's vendor-specific interface and its bulk IN and OUT endpoints

#### Scenario: CDC-ACM with a union descriptor
- **WHEN** a device has an interface association, a CDC control interface 2 with a union functional descriptor naming data interface 3, and other interfaces before them
- **THEN** the host picks the CDC-ACM driver with control interface 2, its interrupt IN endpoint, and the bulk endpoints of interface 3

#### Scenario: CDC-ACM without a union descriptor
- **WHEN** a CDC control interface has no union or call management descriptor
- **THEN** the host pairs it with the next CDC data interface that has one bulk IN and one bulk OUT endpoint

#### Scenario: Not a serial device
- **WHEN** the user starts a serial bridge on a flash drive
- **THEN** it fails saying the device is not a supported USB serial device (CDC-ACM, CP210x, FTDI, CH34x), and the device is not opened

#### Scenario: Descriptors unknown
- **WHEN** the device has no USB permission and so no raw descriptors
- **THEN** the command fails telling the user to run `hubbridge grant <id>` and tap Allow

### Requirement: Opening the port
Opening a serial port SHALL open the device, claim the driver's interfaces with the phone's
kernel driver detached, run the chip's initialisation, apply the requested line settings
and assert DTR and RTS unless asked not to.

#### Scenario: CP210x open at 115200 8N1
- **WHEN** a CP210x port is opened with 115200 baud, 8 data bits, no parity, 1 stop bit
- **THEN** the device receives, on interface 0 in this order, IFC_ENABLE (request 0x00, value 1), SET_BAUDRATE (request 0x1e, data 00 c2 01 00) and SET_LINE_CTL (request 0x03, value 0x0800)

#### Scenario: CDC-ACM open
- **WHEN** a CDC-ACM port with control interface 0 is opened at 115200 8N1
- **THEN** the device receives SET_LINE_CODING (request type 0x21, request 0x20, index 0, data 00 c2 01 00 00 00 08) and SET_CONTROL_LINE_STATE (request 0x22, value 0x0003)

#### Scenario: Open without DTR
- **WHEN** a port is opened with DTR and RTS off
- **THEN** the chip's modem-control request clears both lines and no request asserts them

### Requirement: Baud rate programming per chip
Each driver SHALL program the baud rate the way the chip expects: CDC-ACM and CP210x as a
32-bit rate, FTDI as a divisor of its base clock (3 MHz for FT232R/FT-X/FT2232C/D, 12 MHz
with the high-speed bit for FT2232H/FT4232H/FT232H at 1200 baud and above), CH34x as a
prescaler and divisor of 48 MHz. Rates outside the chip's range SHALL be refused.

#### Scenario: FT232R divisors
- **WHEN** an FT232R port is set to 9600, 115200, 921600 and 3000000 baud
- **THEN** SET_BAUD_RATE (request 0x03) carries value/index 0x4138/0x0000, 0x001a/0x0000, 0x8003/0x0000 and 0x0000/0x0000

#### Scenario: High-speed FTDI divisors
- **WHEN** an FT232H port is set to 3000000 and 12000000 baud
- **THEN** SET_BAUD_RATE carries value/index 0x0004/0x0200 and 0x0000/0x0200

#### Scenario: Second port of an FT2232H
- **WHEN** port B (interface 1) of an FT2232H is set to 115200 baud
- **THEN** SET_BAUD_RATE carries value 0xc068 and index 0x0202

#### Scenario: CH340 divisors
- **WHEN** a CH340 with chip version 0x31 is set to 115200 and 9600 baud
- **THEN** WRITE_REG (request 0x9a, value 0x1312) carries index 0xcc83 and 0xb282

#### Scenario: Rate out of range
- **WHEN** a CP2102 port is asked for 3000000 baud
- **THEN** the request fails naming the chip's maximum (1000000) and the line keeps its previous rate

### Requirement: Line format and flow control
Each driver SHALL apply data bits, parity (none, odd, even, mark, space), stop bits (1,
1.5, 2) and RTS/CTS flow control where the chip supports them, and SHALL report the
settings actually in effect when a request asks for something the chip cannot do.

#### Scenario: FTDI 7E1
- **WHEN** an FTDI port is set to 7 data bits, even parity, 1 stop bit
- **THEN** SET_DATA (request 0x04) carries value 0x0207

#### Scenario: CP210x 7E2
- **WHEN** a CP210x port is set to 7 data bits, even parity, 2 stop bits
- **THEN** SET_LINE_CTL carries value 0x0722

#### Scenario: CH340 line control
- **WHEN** a CH340 with chip version 0x31 is set to 8N1
- **THEN** WRITE_REG (request 0x9a, value 0x2518) carries index 0x00c3

#### Scenario: RTS/CTS on CP210x
- **WHEN** a CP210x port is set to RTS/CTS flow control
- **THEN** SET_FLOW (request 0x13) is sent with CTS handshake enabled and RTS in flow-control mode

#### Scenario: Unsupported setting
- **WHEN** an FTDI port is asked for 5 data bits
- **THEN** the driver reports the settings in effect (8 data bits) instead of claiming 5

### Requirement: Data moves both ways in order
Bytes written to the port SHALL reach the device's bulk OUT endpoint in order and bytes the
device sends on bulk IN SHALL be delivered in order, with several IN transfers kept in
flight so that the next one is already waiting when one completes.

#### Scenario: Loopback round trip
- **WHEN** 64 KiB of patterned data is written to a port whose device loops TX back to RX
- **THEN** exactly the same 64 KiB comes back in the same order

#### Scenario: Idle line
- **WHEN** the device sends nothing for longer than the IN transfer timeout
- **THEN** the timed-out IN transfers are submitted again and later data still arrives, without the port closing

#### Scenario: Stalled IN endpoint
- **WHEN** the bulk IN endpoint stalls
- **THEN** the host clears the halt and keeps reading

### Requirement: FTDI status header is removed from data
The FTDI driver SHALL strip the 2-byte modem/line status header from every IN packet of
max-packet size (64 bytes at full speed, 512 at high speed) and use it as modem and line
status; status-only packets SHALL deliver no data.

#### Scenario: Several packets in one transfer
- **WHEN** an FT2232H at high speed returns a 1024-byte IN transfer made of two 512-byte packets
- **THEN** the port delivers 1020 data bytes, both headers removed

#### Scenario: Status-only packets
- **WHEN** an idle FTDI chip returns 2-byte packets
- **THEN** no data bytes are delivered and the modem status is updated from them

### Requirement: Modem and line status
The port SHALL report CTS, DSR, RI and DCD and the line errors (overrun, parity, framing,
break) the chip makes known: CDC-ACM from SERIAL_STATE notifications on its interrupt
endpoint, CH34x from its interrupt endpoint, FTDI from the in-band header, CP210x by
reading GET_MDMSTS while a consumer asks for modem status.

#### Scenario: CDC-ACM SERIAL_STATE
- **WHEN** a CDC-ACM device sends the notification a1 20 00 00 00 00 02 00 03 00
- **THEN** the port reports DCD and DSR on, RI off, and no line error

#### Scenario: FTDI overrun
- **WHEN** an FTDI IN packet header has the overrun bit set in its line status byte
- **THEN** the port counts one overrun error

#### Scenario: CP210x polling only on demand
- **WHEN** no consumer asks for modem status on a CP210x port
- **THEN** no GET_MDMSTS requests are sent

### Requirement: Modem lines, break and purge
The port SHALL set DTR and RTS on request, send break on and off where the chip supports
it, and purge the chip's receive and transmit buffers where it supports that, discarding
the host's own buffered data in the purged direction.

#### Scenario: DTR off on FTDI
- **WHEN** DTR is turned off on an FTDI port
- **THEN** MODEM_CTRL (request 0x01) carries value 0x0100

#### Scenario: CH34x modem control
- **WHEN** DTR and RTS are both turned on on a CH34x port
- **THEN** MODEM_CTRL (request 0xa4) carries value 0xff9f

#### Scenario: Break on CDC-ACM
- **WHEN** break is turned on and then off on a CDC-ACM port that announces break support
- **THEN** SEND_BREAK (request 0x23) is sent with value 0xffff and then 0

### Requirement: Closing gives the device back
Closing a port SHALL drop DTR and RTS, disable the chip's interface where it has such a
request, release the claimed interfaces and close the device, so the phone and other
commands can use it again; the port SHALL also close when the device is unplugged or the
session ends, reporting why.

#### Scenario: Normal close
- **WHEN** a serial port is closed
- **THEN** the device is closed on the agent and can be opened again by another command

#### Scenario: Device unplugged
- **WHEN** the serial device is unplugged from the phone while the port is open
- **THEN** the port stops within 2 seconds and reports that the device was closed

### Requirement: Counters
The port SHALL count bytes sent and received, received bytes dropped because a consumer
did not keep up, and line errors reported by the chip, and make the counts available while
it runs.

#### Scenario: Counters after a round trip
- **WHEN** 1000 bytes are written to a loopback device and read back
- **THEN** the counters show 1000 bytes sent, 1000 received and 0 dropped
