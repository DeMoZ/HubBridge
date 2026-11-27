# Spec Delta

## Purpose

Defines how the HubBridge desktop app finds newer versions in the GitHub releases of
DeMoZ/HubBridge, tells the user, and replaces itself with the new version safely.

## ADDED Requirements

### Requirement: Version pill in the top bar
The top bar SHALL show a version pill with the installed version (`v0.1.0`). When a newer release is known, the pill SHALL read "v0.2.0 is out" and carry an accent dot. Clicking the pill SHALL open the update sheet.

#### Scenario: Newer release known
- **WHEN** version 0.1.0 runs and the last check found 0.2.0
- **THEN** the pill reads "v0.2.0 is out" with a dot, and clicking it opens the update sheet titled "Version 0.2.0 is available"

### Requirement: Check for updates on request
The update sheet SHALL check the latest release of DeMoZ/HubBridge on "Check for updates" and report one of: a newer version (with its release notes), "You have the latest version, X", or "Couldn't check for updates: <reason>".

#### Scenario: Newer version
- **WHEN** 0.1.0 is installed, the latest release is v0.2.0 with a macOS update file, and the user presses "Check for updates"
- **THEN** the sheet shows "Version 0.2.0 is available", the release notes without their "Install" sections, and the buttons Skip this version, Later and Update and restart

#### Scenario: Up to date
- **WHEN** the latest release is the installed version
- **THEN** the sheet says "You have the latest version, 0.1.0."

#### Scenario: Offline
- **WHEN** there is no network and the user presses "Check for updates"
- **THEN** the sheet shows "Couldn't check for updates:" with curl's reason, and the window keeps working

### Requirement: Release without a file for this system
When the latest release is newer but has no update file for this system, the app SHALL say so and offer the release page instead of installing.

#### Scenario: No Windows file
- **WHEN** the app runs on Windows and the latest release v0.2.0 has only macOS and Android files
- **THEN** the sheet says version 0.2.0 is out but has no download for this system, and offers "Open release page"

### Requirement: Automatic check once a day
When "Check for updates automatically (once a day)" is on (the default), the app SHALL check in the background at start and while running, at most once per 24 hours across restarts, and SHALL open the update sheet when it finds a newer version that the user did not skip. The setting SHALL be saved.

#### Scenario: Background find
- **WHEN** the setting is on, the last check was 25 hours ago, and the app starts while v0.2.0 is published
- **THEN** the app checks without user action and opens the update sheet for 0.2.0

#### Scenario: Checked recently
- **WHEN** the last check was 3 hours ago
- **THEN** no request is made at start; the next check happens when 24 hours have passed, even if the app keeps running

#### Scenario: Turned off
- **WHEN** the user unticks the setting and restarts the app
- **THEN** the setting stays off and the app makes no update request until the user presses "Check for updates"

### Requirement: Skip this version
"Skip this version" SHALL stop the automatic check from opening the sheet for that version; a manual check SHALL still show it, and a newer version than the skipped one SHALL be offered again.

#### Scenario: Skipped 0.2.0
- **WHEN** the user skipped 0.2.0 and the next day's automatic check finds 0.2.0 again
- **THEN** the sheet does not open by itself, but the pill still reads "v0.2.0 is out"

#### Scenario: Newer than skipped
- **WHEN** 0.2.0 was skipped and the automatic check finds 0.3.0
- **THEN** the sheet opens for 0.3.0

### Requirement: Verified download
The app SHALL download the update file, check its SHA-256 against the digest GitHub publishes for that file, and SHALL refuse to install when the digest is missing or different, or when the archive does not contain the HubBridge app, keeping the current version running.

#### Scenario: Tampered file
- **WHEN** the downloaded archive's SHA-256 differs from the published digest
- **THEN** the sheet shows "The update wasn't installed:" with the mismatch, nothing is replaced, and the app keeps running

#### Scenario: Wrong app inside
- **WHEN** the archive contains an app whose bundle id is not `com.demoz.hubbridge`
- **THEN** the update is refused with "the update archive has no HubBridge app inside"

### Requirement: Update and restart
"Update and restart" SHALL download and verify the update, then stop running serial bridges, disconnect from the phone, quit, replace the installed app with the new one and start it. If replacing fails, the previous app SHALL be put back and started, and it SHALL say that the update did not install.

#### Scenario: Successful update
- **WHEN** `HubBridge.app` 0.1.0 in a writable folder updates to 0.2.0
- **THEN** the window closes, `HubBridge.app` in the same folder is 0.2.0 and starts by itself

#### Scenario: Replace fails
- **WHEN** the new app cannot be moved into place
- **THEN** the old app is back in its folder, starts again and shows "The update didn't install, so the previous version is running. Download the new one from the release page."

#### Scenario: Serial bridge running
- **WHEN** a serial bridge is running and the user opens the update sheet with a newer version
- **THEN** the sheet warns that running serial bridges will stop, and after "Update and restart" the bridge's PTY / TCP port is closed before the app quits

### Requirement: Update refused while the phone is busy
The app SHALL refuse to start an update while it installs the app on a phone or pairs with a phone, and SHALL say to wait.

#### Scenario: APK install running
- **WHEN** "Install app" is still running for a phone and the user presses "Update and restart"
- **THEN** the sheet says to wait until the phone action finishes, and nothing is downloaded

### Requirement: Only the packaged app updates itself
Updating SHALL work only for the app installed from a release (`HubBridge.app` on macOS, the folder with `HubBridge.exe` on Windows). A development build SHALL say that updating works only in the built app and SHALL disable "Check for updates"; a copy in a folder the user cannot write to SHALL offer the release page instead.

#### Scenario: cargo run
- **WHEN** the window runs from `cargo run`
- **THEN** the sheet says "Updating works only in the built app (from the release .dmg or .zip)." and "Check for updates" is disabled

#### Scenario: Read-only folder
- **WHEN** the app runs from a folder it cannot write to and a newer version exists
- **THEN** the sheet says this copy can't update itself and offers "Open release page" instead of "Update and restart"

### Requirement: Release pages open in the browser
"Open release page" SHALL open the new version's release page, and "All versions" the releases list of DeMoZ/HubBridge, in the default browser.

#### Scenario: All versions
- **WHEN** the user presses "All versions"
- **THEN** the browser opens `https://github.com/DeMoZ/HubBridge/releases`
