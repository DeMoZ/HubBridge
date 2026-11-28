# donations Specification

## Purpose
Defines how a user can support HubBridge's development from the desktop window: the Support
button, the sheet with donation methods, and where its links and texts come from, so they can
change without a new release.

## Requirements

### Requirement: Support pill in the top bar
The top bar SHALL show a "♥ Support" pill next to the version pill, highlighted in the support color on hover. Clicking it SHALL open the Support sheet.

#### Scenario: Open the sheet
- **WHEN** the user clicks "♥ Support"
- **THEN** a sheet titled "Thanks for using HubBridge!" opens over the window

### Requirement: Support sheet content
The sheet SHALL show the message and the donation methods in the order of the file, each as a row with its title, optional description and an outward arrow. Clicking a row SHALL open its payment page in the default browser. Escape, the close button and a backdrop click SHALL close the sheet.

#### Scenario: Two methods
- **WHEN** the file lists Tribute and CloudTips
- **THEN** the sheet shows both rows in that order, and clicking the CloudTips row opens `https://pay.cloudtips.ru/p/902c7c74` in the browser

### Requirement: Methods come from the donations repository
Each time the sheet opens, the app SHALL download `hubbridge.json` (schema 1) from DeMoZ/donations: first `raw.githubusercontent.com`, then the jsDelivr mirror. A valid file SHALL be saved as the cached copy. Until the download ends the sheet SHALL show the cached copy, or the copy built into the app, with a spinner by the title.

#### Scenario: Fresh file
- **WHEN** the sheet opens with the network up and the file changed in the repository since the last time
- **THEN** the sheet first shows the saved copy with a spinner, then the new texts and links without the spinner

#### Scenario: Offline with a saved copy
- **WHEN** the sheet opens without a network and a copy was saved before
- **THEN** the sheet shows the saved copy and the note "Couldn't load the latest list, showing a saved copy."

#### Scenario: Offline on first start
- **WHEN** the sheet opens without a network and nothing was ever saved
- **THEN** the sheet shows the methods built into the app and the same note

### Requirement: Safe links and lenient parsing
Unknown fields SHALL be ignored; a method without a title or without an `https://` link SHALL be dropped. A file with no usable method SHALL be treated as invalid and SHALL NOT replace the saved copy. The app SHALL open only `https://` links.

#### Scenario: Unsafe link
- **WHEN** the file has methods with `http://…` and `javascript:…` links next to a valid one
- **THEN** only the valid method is shown

#### Scenario: Broken file
- **WHEN** the downloaded file is not JSON
- **THEN** the saved copy stays as it was and the sheet keeps showing it

### Requirement: Texts in the window's language
Texts SHALL be chosen for the window's language: the exact tag, then its base language, then English; a plain string counts as English.

#### Scenario: Russian window
- **WHEN** the window is in Russian and the file has `en` and `ru` texts
- **THEN** the sheet shows the `ru` texts

#### Scenario: Missing translation
- **WHEN** a method's description exists only in `en`
- **THEN** the Russian window shows the English description for it
