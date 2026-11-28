# Spec Delta

## Purpose

Shows the Mac's screen on a display attached to the phone: the Mac captures and encodes its main display, the
phone decodes the stream onto the external monitor, with bounded delay, clear start/stop, live stats and a
visible reason whenever mirroring ends.

## ADDED Requirements

### Requirement: Start mirroring onto a phone display
On macOS, with a connected phone app that knows displays, the user SHALL be able to start mirroring the Mac's
main display onto a chosen phone display from the window or with `hubbridge mirror`; the picture SHALL appear on
that display within three seconds of the start being accepted.

#### Scenario: Start from the window
- **WHEN** the window is connected to the phone, the monitor is listed in "Phone displays" and the user presses Mirror
- **THEN** within three seconds the monitor shows the Mac's main display and the panel shows the mirror as running with its stats

#### Scenario: Start from the CLI
- **WHEN** `hubbridge mirror --display <id>` runs against a connected phone
- **THEN** the monitor shows the Mac's screen, the command prints stats once per second and runs until Ctrl-C, which stops the mirror

### Requirement: Picture fits the display without distortion
The picture SHALL keep the Mac display's aspect ratio: it is scaled to the largest size that fits the phone
display's current resolution (never above it and never above 1920×1080), with even width and height, centered,
the rest of the monitor black. The picture SHALL never be stretched or cropped.

#### Scenario: 16:10 Mac on a 1280×720 monitor
- **WHEN** a Mac display of 3024×1964 pixels is mirrored to a 1280×720 display
- **THEN** the stream is 1108×720 and the monitor shows it centered with black bars left and right

#### Scenario: 16:9 Mac
- **WHEN** a 2560×1440 Mac display is mirrored to a 1280×720 display
- **THEN** the stream is 1280×720 and fills the monitor

#### Scenario: Monitor mode changes while mirroring
- **WHEN** the monitor's mode changes during mirroring
- **THEN** the picture stays centered and undistorted on the new mode, without a restart

### Requirement: Screen content only over the authenticated encrypted link
Captured screen content SHALL leave the Mac only inside sessions authenticated with the key saved at pairing and
encrypted like every other HubBridge session; the phone SHALL show a stream only to the paired host that started
it on its own session; nothing is written to disk except a file the user names on the command line.

#### Scenario: Video session of another host
- **WHEN** a second paired host opens a session and tries to attach to a stream started by the first host
- **THEN** the phone refuses the attach and the first host's mirror keeps running

#### Scenario: Unpaired client
- **WHEN** a client without the saved key connects to the phone's port
- **THEN** it is rejected in the handshake as before and receives no video

### Requirement: Video does not delay control traffic
Video SHALL travel on its own session to the phone, separate from the session that carries pings, events and USB
requests, so that a large frame never waits in front of a control message on the same connection.

#### Scenario: Round trip during mirroring
- **WHEN** the simulator is mirrored at 30 frames per second over loopback
- **THEN** the control session's round-trip time stays under 20 ms and the serial bridge and USB requests still work

### Requirement: Bounded delay through frame skipping
The Mac SHALL keep at most two frames sent but not yet acknowledged by the phone's decoder input; while that limit
is reached it SHALL skip captured frames instead of queueing them, always encoding the newest captured frame
next, and count every skipped frame.

#### Scenario: Slow phone decoder
- **WHEN** the simulator acknowledges frames only every 100 ms while the Mac captures 30 frames per second
- **THEN** no more than two frames are ever unacknowledged, the skipped-frames counter grows, and the delay from capture to acknowledgement stays below 400 ms

#### Scenario: Last change is shown
- **WHEN** the screen changes once while the limit is reached and then stays still
- **THEN** that last change is encoded and reaches the phone once an acknowledgement frees the window

### Requirement: Keyframes when needed
The stream SHALL start with the decoder configuration and a keyframe; the Mac SHALL send a new keyframe within
half a second when the phone asks for one, and at least every ten seconds; forced keyframes SHALL be limited to
two per second.

#### Scenario: Phone asks for a keyframe
- **WHEN** the phone's decoder reports an error and asks for a keyframe
- **THEN** the next frame the Mac sends within 500 ms is a keyframe and the picture recovers

#### Scenario: Still screen and a keyframe request
- **WHEN** the Mac screen is still and the phone asks for a keyframe
- **THEN** the Mac encodes its last captured picture as a keyframe and sends it

### Requirement: A still screen keeps the mirror alive
A still Mac screen produces no frames; the mirror SHALL stay running indefinitely while the screen is still, and
the phone SHALL keep showing the last picture.

#### Scenario: Idle for a minute
- **WHEN** the Mac screen does not change for 60 seconds during mirroring
- **THEN** the mirror is still running, the monitor still shows the last picture, and the next change appears on it

### Requirement: Stop mirroring
The user SHALL be able to stop a mirror from the window (Stop) or by ending `hubbridge mirror`; the phone SHALL
remove the picture from the display within one second, the video session SHALL close, and the control session
SHALL stay connected.

#### Scenario: Stop from the window
- **WHEN** the user presses Stop while mirroring
- **THEN** within one second the monitor no longer shows the Mac, capture stops on the Mac, and the window is still connected to the phone

### Requirement: Mirror ends when the display goes away
When the mirrored display is unplugged or removed, the phone SHALL end that mirror and tell the host why; the
Mac SHALL stop capturing and the window SHALL show that the monitor was disconnected.

#### Scenario: Monitor unplugged while mirroring
- **WHEN** the monitor is unplugged from the phone during mirroring
- **THEN** within two seconds the Mac stops capturing and the panel shows "Mirroring stopped: the monitor was disconnected"

### Requirement: Mirror ends with the connection
When the control session or the video session ends (network loss, phone app stopped, phone restarted), mirroring
SHALL stop on both sides, the Mac SHALL stop capturing, and the window SHALL show the reason. Mirroring SHALL NOT
restart by itself after the window reconnects.

#### Scenario: Wi-Fi lost during mirroring
- **WHEN** the phone loses Wi-Fi while mirroring over Wi-Fi
- **THEN** the Mac stops capturing within six seconds, the panel shows that mirroring stopped because the connection was lost, and after the window reconnects the mirror is not running until the user starts it again

#### Scenario: Only the video session drops
- **WHEN** the video session closes while the control session stays up
- **THEN** the phone removes the picture, the host is told why on the control session, and the panel shows the reason

### Requirement: Mirror ends when the phone cannot show it
When the phone cannot show or decode the stream (the decoder fails and does not recover after a keyframe, stops
accepting frames for two seconds, or the surface on the display is taken away), it SHALL end the mirror and tell
the host the reason in words.

#### Scenario: Decoder stops accepting frames
- **WHEN** the phone's decoder accepts no frame for two seconds
- **THEN** the phone ends the mirror and the panel shows "Mirroring stopped: the phone's video decoder stopped"

### Requirement: Start refused with a reason
A start SHALL be refused with a message that says what to do when: the display id is unknown, the display
already shows a mirror, the phone cannot show a picture on it under its current conditions (as the spike found,
e.g. the HubBridge app is not open on the phone), the codec or size is unsupported, or the Mac denies Screen
Recording.

#### Scenario: Display already mirrored
- **WHEN** a second start targets a display that already shows a mirror
- **THEN** the start is refused with "this display is already showing a mirror" and the running mirror continues

#### Scenario: Unknown display
- **WHEN** a start names a display id the phone does not have
- **THEN** the start is refused with "the phone has no display <id>"

### Requirement: Stats readout
While mirroring, the window SHALL update once per second, and `hubbridge mirror` SHALL print once per second:
stream size and codec, frames sent per second, bitrate, frames skipped, keyframes, the delay from capture to the
phone's acknowledgement, the video session's round-trip time, and the phone's decoded frames per second and
dropped frames.

#### Scenario: Stats while mirroring the simulator
- **WHEN** the simulator is mirrored with a changing screen for five seconds
- **THEN** the readout shows a non-zero frame rate and bitrate, a delay value in milliseconds and the simulator's decoded frame rate

#### Scenario: Still screen
- **WHEN** the screen is still
- **THEN** the readout shows 0 frames per second without an error

### Requirement: Screen Recording permission flow
Before capturing, the Mac SHALL check the Screen Recording permission without prompting; when it is missing it
SHALL ask macOS once and the window SHALL explain how to allow HubBridge in System Settings, offer to open that
settings page, and say that the app must be reopened after allowing it.

#### Scenario: First start without permission
- **WHEN** the user presses Mirror and HubBridge has no Screen Recording permission
- **THEN** macOS shows its permission request (first time only), mirroring does not start, and the window shows the explanation with an "Open System Settings" button

#### Scenario: Permission granted and app reopened
- **WHEN** the user has allowed HubBridge in System Settings and reopened the app
- **THEN** pressing Mirror starts mirroring without any message about permission

### Requirement: The phone shows that it is mirroring
While a mirror runs, the phone's HubBridge notification SHALL say which computer's screen is shown on which
display, and the phone app SHALL not count the video session as a second connected computer.

#### Scenario: Notification while mirroring
- **WHEN** a Mac named "MacBook" mirrors to "HDMI Screen"
- **THEN** the phone's notification says that MacBook's screen is shown on HDMI Screen, and the connected-computers count stays one

### Requirement: Compatible with older apps
Mirroring SHALL add messages without changing the protocol version, the hellos, key derivation or record framing;
a host with this change and a phone app without it, or the reverse, SHALL keep pairing, connecting and using USB
devices as before.

#### Scenario: New window, old phone app
- **WHEN** the window with this change connects to a phone app of version 0.1.0
- **THEN** it never sends a display or mirror message to it, shows that the phone app must be updated for mirroring, and everything else works

#### Scenario: Old window, new phone app
- **WHEN** a window of version 0.1.0 connects to a phone app with this change and a monitor is plugged in
- **THEN** the session keeps working and the old window receives no message it cannot decode

### Requirement: Mirroring needs macOS for now
On Windows the window and the CLI SHALL list phone displays but SHALL say that mirroring needs macOS for now,
instead of offering a start that fails.

#### Scenario: Windows window
- **WHEN** the window runs on Windows and is connected to a phone with a monitor
- **THEN** the display is listed and the Mirror button is replaced by the text "Mirroring needs macOS for now"

### Requirement: Quitting or updating stops mirroring first
When the window quits or closes for an update while mirroring, it SHALL stop the mirror (the phone removes the
picture) before disconnecting; no mirror SHALL start once the app has begun closing for an update.

#### Scenario: Update while mirroring
- **WHEN** the user installs an update while a mirror runs
- **THEN** the mirror stops, the phone removes the picture, and the app quits and updates

### Requirement: Testing without a phone
The simulator SHALL accept mirror streams on its fake displays, acknowledge frames (optionally after a configured
delay), check that every frame is well-formed H.264 Annex B with decoder configuration before the first keyframe,
report decoded-frame stats, and optionally write the received stream to a file playable by standard tools.

#### Scenario: Mirror into the simulator's file
- **WHEN** `hubbridge mirror` runs for five seconds against the simulator started with `--video-out <file>`
- **THEN** the file holds an H.264 stream whose first access unit has SPS, PPS and an IDR slice, and `ffprobe` (if installed) reports the stream's width and height

#### Scenario: Malformed frame
- **WHEN** the simulator receives a frame that is not Annex B or a non-key frame before any keyframe
- **THEN** it counts the frame as bad and asks for a keyframe
