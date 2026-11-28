# Spec Delta

## MODIFIED Requirements

### Requirement: Status is never shown by color alone
Each state shown with a color (online/offline, paired/not paired, permission, link state, transport readiness, error lines) SHALL also be shown by text or a distinct shape.

#### Scenario: Offline agent
- **WHEN** a paired agent the window is not connected to is no longer seen on the network
- **THEN** its card shows the words "not seen on the network" in a chip, not only a grey dot

#### Scenario: Connected agent not seen by mDNS
- **WHEN** the window is connected to a paired agent over ADB (127.0.0.1) and mDNS does not see that agent
- **THEN** its card shows "connected · ADB" in a green chip, never "offline" or "not seen on the network"

#### Scenario: Connected over Wi-Fi
- **WHEN** the window is connected to an agent over Wi-Fi
- **THEN** its card shows "connected · Wi-Fi"

#### Scenario: Seen but not connected
- **WHEN** mDNS sees a paired agent the window is not connected to
- **THEN** its card shows "on the network"

#### Scenario: Local Network hint on macOS
- **WHEN** on macOS a paired agent is not seen on the network
- **THEN** the card also says that macOS may be blocking HubBridge's Local Network access and where to allow it (System Settings → Privacy & Security → Local Network)

### Requirement: Layout regions of the window
The window SHALL have a top bar (brand mark and name on the left, connection state in the middle, an action area on the right) and a body with a left column (phones on the network, phones over USB and ADB) and a right column (the connection, then the phone's displays, then the phone's USB devices). Every action of the previous design SHALL stay reachable.

#### Scenario: Smallest window size
- **WHEN** the window is at its minimum size of 900×600
- **THEN** both columns are visible side by side, nothing is cut off horizontally, and each column scrolls on its own

#### Scenario: Every action still there
- **WHEN** the simulator is connected and shows its fake devices
- **THEN** Connect, Disconnect, Pair, Forget, Connect by IP, Install app, Connect via ADB, ADB over Wi-Fi, wireless-debugging pairing, Ask phone, Details and the serial Start/Stop/Copy controls can all be reached and work as before

#### Scenario: Phone displays panel
- **WHEN** the simulator with a fake display is connected
- **THEN** the right column shows the "Phone displays" panel between the connection and the USB devices, with the display and its Mirror button (macOS) reachable by keyboard

## ADDED Requirements

### Requirement: One link chip per kind for each phone
In the "USB and ADB" panel each phone SHALL show at most one "USB" chip and one "Wi-Fi" chip, however many adb
transports of that kind it has; a chip is ready (green) when any transport of its kind is ready, its tooltip
lists every transport serial of that kind, and actions use a ready transport, preferring USB, then `ip:port`.

#### Scenario: Two network transports of one phone
- **WHEN** adb lists `192.168.1.108:5555` and `adb-R28M303RHET-LfTN6A._adb-tls-connect._tcp.` for the same phone, both ready
- **THEN** the phone's card shows one green "Wi-Fi" chip whose tooltip names both serials

#### Scenario: One of two not ready
- **WHEN** one network transport is ready and the other is offline
- **THEN** the card shows one green "Wi-Fi" chip and the offline transport's hint line

#### Scenario: Nothing of a kind is ready
- **WHEN** the only USB transport is unauthorized
- **THEN** the card shows one "USB · unauthorized" chip in the warning style with its hint
