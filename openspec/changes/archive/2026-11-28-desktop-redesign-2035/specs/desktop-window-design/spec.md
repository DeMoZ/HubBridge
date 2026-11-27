# Spec Delta

## Purpose

Defines how the HubBridge desktop window looks and responds visually on macOS and Windows:
themes, layout regions, component states, motion, accessibility and the app icon, separate from
the data and actions the window offers.

## ADDED Requirements

### Requirement: Theme follows the system appearance
The window SHALL show a dark theme when the system appearance is dark and a light theme when it is light, switching live when the system setting changes, without a restart and without losing typed text or open sheets.

#### Scenario: System switches to light while the window is open
- **WHEN** the window is open in the dark theme and the user switches macOS (or Windows) to the light appearance
- **THEN** the window redraws in the light theme within a second, and a half-typed PIN in the open pairing sheet is still there

#### Scenario: Start in light appearance
- **WHEN** the window starts while the system appearance is light
- **THEN** the first frame of the page already uses the light theme (no dark flash of the page content)

### Requirement: Text contrast in both themes
Every text color used for content SHALL have a contrast ratio of at least 4.5:1 against the surfaces it is drawn on, in both themes, including secondary and faint text, status colors (ok, warning, error, info) and text on the accent-filled primary button.

#### Scenario: Faint text in the light theme
- **WHEN** a card shows its faint sub-line (for example an agent's address) in the light theme
- **THEN** the measured contrast of that text against the card surface is at least 4.5:1

#### Scenario: Error line in the dark theme
- **WHEN** a serial bridge shows "Stopped: …" in the error color in the dark theme
- **THEN** the measured contrast against the card surface is at least 4.5:1

### Requirement: Status is never shown by color alone
Each state shown with a color (online/offline, paired/not paired, permission, link state, transport readiness, error lines) SHALL also be shown by text or a distinct shape.

#### Scenario: Offline agent
- **WHEN** a paired agent goes offline
- **THEN** its card shows the word "offline" in a chip, not only a grey dot

### Requirement: Layout regions of the window
The window SHALL have a top bar (brand mark and name on the left, connection state in the middle, an action area on the right) and a body with a left column (phones on the network, phones over USB and ADB) and a right column (the connection, then the phone's USB devices). Every action of the previous design SHALL stay reachable.

#### Scenario: Smallest window size
- **WHEN** the window is at its minimum size of 900×600
- **THEN** both columns are visible side by side, nothing is cut off horizontally, and each column scrolls on its own

#### Scenario: Every action still there
- **WHEN** the simulator is connected and shows its fake devices
- **THEN** Connect, Disconnect, Pair, Forget, Connect by IP, Install app, Connect via ADB, ADB over Wi-Fi, wireless-debugging pairing, Ask phone, Details and the serial Start/Stop/Copy controls can all be reached and work as before

### Requirement: Connection state in the top bar
The top bar SHALL show the connection state at all times: not connected, connecting or reconnecting to a named target, needs pairing, or connected with the phone's name, the link type (Wi-Fi or ADB) and the latest round-trip time.

#### Scenario: Connected over Wi-Fi
- **WHEN** the window is connected to "Pixel 8" over Wi-Fi with a last round trip of 12.4 ms
- **THEN** the top bar shows a steady "connected" mark, "Pixel 8", "Wi-Fi" and "12.4 ms"

#### Scenario: Reconnecting
- **WHEN** the link drops and the window reconnects
- **THEN** the top bar shows "Reconnecting to …" with a mark distinct from the connected one

### Requirement: Action area in the top bar
The right side of the top bar SHALL hold an action area for app-level buttons (version and updates, Support) that stays clear of the window-drag region and of the macOS window buttons.

#### Scenario: Dragging the window on macOS
- **WHEN** the user presses on an empty part of the top bar and drags
- **THEN** the window moves; pressing on a button in the action area does not start a drag

### Requirement: Visible keyboard focus
Every interactive element (buttons, fields, checkboxes, disclosure summaries) SHALL show a focus indicator when reached by the keyboard, with at least 3:1 contrast against its surroundings, and SHALL NOT show it on a mouse click. Text fields are the exception: they show their focus ring whenever focused, including by mouse click, because the ring also marks where typing goes.

#### Scenario: Tabbing through a card
- **WHEN** the user presses Tab to move into an agent card
- **THEN** the focused button shows a clearly visible ring; clicking the same button with the mouse shows no ring

### Requirement: Sheets close from the keyboard and the backdrop
A modal sheet SHALL close on Escape and on a click on its backdrop, and SHALL put the keyboard focus on its first field when it opens.

#### Scenario: Pairing sheet
- **WHEN** the pairing sheet is open and the user presses Escape
- **THEN** the sheet closes and nothing is sent to the phone

### Requirement: Reduced motion
When the system asks for reduced motion, the window SHALL stop all continuous animation (the ambient background, pulsing state marks, spinners turn into a static busy mark) and SHALL replace movement in transitions with short fades.

#### Scenario: Reduce motion on
- **WHEN** "Reduce motion" is on in the system settings and the window is connecting
- **THEN** nothing on the page moves continuously, and the connecting state is still shown by text and a static mark

### Requirement: Increased contrast and reduced transparency
When the system asks for more contrast or for reduced transparency, the window SHALL draw its panels, cards and sheets on opaque surfaces without background blur, with stronger borders.

#### Scenario: Reduce transparency on Windows
- **WHEN** transparency effects are off in Windows settings
- **THEN** panels and sheets are opaque and nothing behind them shows through

### Requirement: Self-contained page
The page SHALL load nothing from the network: no web fonts, scripts, styles or images from a URL. It SHALL use the system fonts of the platform.

#### Scenario: No network
- **WHEN** the computer has no network connection and the window starts
- **THEN** the window looks the same as with a network connection

### Requirement: Application icon
The desktop app SHALL have an icon with the HubBridge mark, provided as a macOS icon set and a 1024×1024 PNG generated from one source script, and the Android app's launcher icon SHALL use the same mark and colors.

#### Scenario: Icon in the Dock
- **WHEN** the packaged `HubBridge.app` runs
- **THEN** the Dock and Finder show the HubBridge mark, not the generic application icon

#### Scenario: Launcher icon on the phone
- **WHEN** the HubBridge APK built from this change is installed on a phone
- **THEN** the launcher shows the same mark as the desktop icon
