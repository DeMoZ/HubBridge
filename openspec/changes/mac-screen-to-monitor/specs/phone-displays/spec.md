# Spec Delta

## Purpose

Lets the computer see the external displays attached to the phone (a monitor on the USB-C port or hub), with
their resolution and refresh rate, kept up to date when a monitor is plugged in or out, so that one can be
chosen as a mirror target.

## ADDED Requirements

### Requirement: The phone reports its presentation displays
The phone app SHALL report every display Android lists in the presentation category (external monitors, not the
phone's own screen and not Samsung DeX's internal desktop) with: display id, name, current width and height in
pixels, current refresh rate in millihertz, the supported modes (width, height, refresh rate), whether it is a
presentation display, whether it is secure, and whether Samsung DeX is on.

#### Scenario: Monitor on the USB-C port
- **WHEN** the host asks the phone with a monitor on its USB-C port or hub for its displays
- **THEN** the answer lists one display named as Android names it (e.g. "HDMI Screen") with the width, height and refresh rate of its current mode as Android reports them at that moment (they and the display id can differ between DeX on and off and after a replug), its supported modes, and the DeX flag as the phone reports it

#### Scenario: No monitor
- **WHEN** the host asks a phone without an external display
- **THEN** the answer is an empty display list, not an error

### Requirement: Display hot-plug events
After a host session has asked for the displays once, the agent SHALL send that session an event with the full
display list whenever a display is added, removed or changes its mode or name, within one second of Android
reporting the change.

#### Scenario: Monitor plugged in
- **WHEN** a session has asked for displays and a monitor is then plugged into the phone
- **THEN** that session receives a display-list event containing the new display within one second

#### Scenario: Monitor unplugged
- **WHEN** a session has asked for displays and the monitor is unplugged
- **THEN** that session receives a display-list event without it

### Requirement: Display events only for hosts that asked
The agent SHALL NOT send a display-list event to a session that has never asked for the displays, so that a host
built before this change never receives a message it cannot decode.

#### Scenario: Old host connected
- **WHEN** a host that never asks for displays is connected and a monitor is plugged into the phone
- **THEN** the host receives no display event and its session keeps working (pings, device events, USB requests)

### Requirement: Hosts ask only phone apps that know displays
A host SHALL ask for displays only when the phone app's reported version is at least the first version with this
change; for an older phone app it SHALL show that the phone app must be updated to use displays, and keep the
session open.

#### Scenario: Old phone app
- **WHEN** the window connects to a phone app of version 0.1.0
- **THEN** the "Phone displays" panel says the phone app is too old for displays and offers Install/Update app when the phone is on adb, and the session stays connected

### Requirement: Simulated displays
The agent simulator SHALL accept fake displays on its command line (name, resolution, refresh rate) and SHALL be
able to plug and unplug its first fake display on a timer, so the display list and hot-plug can be tested
without a phone.

#### Scenario: Simulator with a fake monitor
- **WHEN** the simulator runs with a fake display "HDMI Screen" at 1280×720@60 and a host asks for displays
- **THEN** the answer lists that display with those values

#### Scenario: Simulated hot-plug
- **WHEN** the simulator runs with display churn every 2 seconds and a host has asked for displays
- **THEN** the host receives display-list events that alternately contain and lack the first display

### Requirement: Displays in the desktop window
While connected to a phone app that knows displays, the window SHALL show a "Phone displays" panel listing each
display with its name, resolution and refresh rate (e.g. "1280×720 · 60 Hz"), updated live on hot-plug, and a
line saying when Samsung DeX is on.

#### Scenario: Live update in the window
- **WHEN** the window is connected and the monitor is unplugged from the phone
- **THEN** the display disappears from the panel within two seconds and the panel shows that no display is attached

### Requirement: Displays in the CLI
`hubbridge displays [target]` SHALL print the phone's displays (id, name, resolution, refresh rate, presentation
and secure flags, DeX state), and `--json` SHALL print the same data as JSON.

#### Scenario: CLI listing
- **WHEN** `hubbridge displays` runs against the simulator with one fake display
- **THEN** it prints one line with the display's id, name, "1280×720", "60 Hz" and exits 0
