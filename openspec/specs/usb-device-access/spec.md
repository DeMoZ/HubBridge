# usb-device-access Specification

## Purpose

Lets a paired desktop host use a USB device attached to the phone as if it were local:
open it, claim interfaces and exchange control, bulk and interrupt transfers through the
agent's encrypted session.

## Requirements

### Requirement: Opening a device needs the phone's USB permission
The agent SHALL open a listed device for a session only when the user has granted USB
access to it on the phone, and SHALL answer `NoPermission` otherwise.

#### Scenario: Permission granted
- **WHEN** a host sends `OpenDevice` for a listed device the user has allowed
- **THEN** the agent answers `Done` and the device is open for that session

#### Scenario: Permission missing
- **WHEN** a host sends `OpenDevice` for a device without USB permission
- **THEN** the agent answers `Error` with code `NoPermission`

#### Scenario: Unknown device
- **WHEN** a host sends `OpenDevice` with an id that is not in the device list
- **THEN** the agent answers `Error` with code `UnknownDevice`

### Requirement: One session owns an open device
An open device SHALL belong to the session that opened it. Other sessions SHALL get `Busy`
when opening it and `NotOpen` for any other request on it.

#### Scenario: Second computer
- **WHEN** host B sends `OpenDevice` for a device host A has open
- **THEN** host B gets `Error` with code `Busy` and host A keeps the device

#### Scenario: Request on a device not opened by this session
- **WHEN** a host sends `Control`, `Transfer` or another device request for a device it has not opened
- **THEN** it gets `Error` with code `NotOpen`

### Requirement: Claiming an interface can take it from the phone
`ClaimInterface` SHALL claim the interface for the session; with `detach_kernel_driver` set
the agent SHALL first detach the phone's own driver from it.

#### Scenario: Claim with detach
- **WHEN** a host claims the mass-storage interface of a flash drive with `detach_kernel_driver`
- **THEN** the agent answers `Done`, and the phone no longer uses that interface while it is claimed

#### Scenario: Bulk transfer without a claimed interface
- **WHEN** a host sends `Transfer` on an endpoint whose interface is not claimed
- **THEN** the agent answers `TransferDone` with status `InvalidArgument`

### Requirement: Control transfers on endpoint 0
The agent SHALL perform a `Control` request on the device's default endpoint, reading
`setup.length` bytes for IN (bit 7 of `request_type`) or sending `data` for OUT, and SHALL
answer `ControlDone` with the device's status and the IN data.

#### Scenario: Live device descriptor
- **WHEN** a host sends a standard GET_DESCRIPTOR(device) control request of 18 bytes
- **THEN** the agent answers `ControlDone` with status `Ok` and the device's 18-byte device descriptor

#### Scenario: Device rejects the request
- **WHEN** the device stalls a control request
- **THEN** the agent answers `ControlDone` with status `Stall`

### Requirement: Bulk and interrupt transfers complete in request order
For each endpoint the agent SHALL pass `Transfer` requests to the device in the order they
arrived and answer each with `TransferDone`; a host MAY keep several in flight. IN reads up
to `length` bytes, OUT sends `data`; one transfer is at most 1 MiB.

#### Scenario: Several writes and reads in flight
- **WHEN** a host sends ten OUT transfers to a loopback device without waiting, then reads back
- **THEN** each OUT is answered `Ok` with its byte count and the bytes come back in the order sent

#### Scenario: Read with no data before the timeout
- **WHEN** an IN transfer gets no data within its `timeout_ms`
- **THEN** the agent answers `TransferDone` with status `Cancelled`

#### Scenario: Oversized transfer
- **WHEN** a host requests a transfer larger than 1 MiB
- **THEN** the agent answers `Error` with code `Unsupported`

### Requirement: Endpoint and device recovery
The agent SHALL support `ClearHalt` on an endpoint and `ResetDevice`, answering `Done` on
success and `Error` with code `UsbFailed` when the OS refuses.

#### Scenario: Clear a stalled endpoint
- **WHEN** a bulk IN endpoint stalled and the host sends `ClearHalt` for it
- **THEN** the agent answers `Done` and later transfers on that endpoint run again

### Requirement: Open devices close with their owner or the device
The agent SHALL close a device when its session sends `CloseDevice`, when the session ends,
or when the device is unplugged; on unplug it SHALL send the event `DeviceClosed` with a reason.

#### Scenario: Unplugged while open
- **WHEN** a device that a host has open disappears from the phone
- **THEN** the host receives `DeviceClosed` for it and later requests on it get `NotOpen`

#### Scenario: Session ends
- **WHEN** the session that opened a device disconnects
- **THEN** the agent closes the device and another host can open it

### Requirement: Failures inside the agent still answer
A USB request that fails inside the agent SHALL be answered with `Error` code `Internal`,
not left without a reply.

#### Scenario: Backend fault while opening
- **WHEN** opening a device fails unexpectedly inside the agent
- **THEN** the host gets `Error` with code `Internal` instead of a request timeout

### Requirement: Bounded pending requests per session
The agent SHALL keep at most 256 USB requests pending per session and SHALL stop reading
new requests from that session until one completes.

#### Scenario: Host floods requests
- **WHEN** a host has 256 USB requests pending and sends more
- **THEN** the agent handles the extra requests only as pending ones complete, and none are dropped
