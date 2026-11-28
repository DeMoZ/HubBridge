# Proposal

## Open questions

Defaults are taken from UsbMemoryChecker (the closest sibling: Rust + tao + wry) so the plan can
go ahead; each one can be changed before `/opsx:apply`.

1. **Donation file in DeMoZ/donations.** The repo has no `hubbridge.json` yet. Default: this
   change prepares it (same content as the built-in copy) and opens a PR there adding the file
   and its README row; merging it is Roman's step. Until it is merged the Support sheet shows the
   built-in copy with the "showing a saved copy" note. Say if the PR should not be opened by the
   agent.
2. **Donation methods and texts.** Default: the same two methods and links as every other DeMoZ
   tool (Tribute `https://web.tribute.tg/d/T3o`, CloudTips `https://pay.cloudtips.ru/p/902c7c74`),
   texts in English and Russian (the window's two languages), message "HubBridge is free. If it
   saves you a cable or a trip to your desk, you can support its development."
3. **Check interval while running.** UsbMemoryChecker checks once at start when the last check
   is a day old. HubBridge may run for days (a serial bridge, later a tray app), so the default
   adds an hourly timer that applies the same once-a-day rule (as UniversalKM's spec "not more
   than once a day, in the background").
4. **Windows.** There is no Windows release (`release-pipeline`). Default: the Windows code path
   (asset `-windows-x64.zip`, PowerShell replace script) is kept and compiled, and a release
   without a Windows file is reported as "Version X is out, but it has no download for this
   system" with the release page button, instead of UsbMemoryChecker's silent "up to date".
5. **Order.** Recommended after `desktop-redesign-2035` (uses its `.pill`, `.sheet`, action
   slot and tokens) and `release-pipeline` (asset names). The end-to-end update test needs two
   published releases, so it is a follow-up after v0.1.1.

## Why

Roman wants the first HubBridge release to update itself from GitHub releases and to have the
Support button, "the same way as in the neighbouring projects" (ROADMAP step 4: "Updates and
Support button like the other DeMoZ apps"). Without an update check, people who download v0.1.0
never learn about fixes — and the bundled phone app (`release-pipeline`) would go stale with it.

## What Changes

- **Version pill and update sheet** in the window's top bar, as in UsbMemoryChecker: the pill
  shows `v0.1.0`, or "v0.2.0 is out" with a dot when a newer release exists; the sheet shows the
  installed version, the new version's notes, a status line, "Check for updates automatically
  (once a day)", and the buttons Skip this version / Later / Update and restart (or Open release
  page) or All versions / Check for updates.
- **Update from GitHub releases**: `releases/latest` of `DeMoZ/HubBridge`, the asset for this
  system (`-macos-arm64.zip`), SHA-256 checked against GitHub's digest, unpacked with `ditto`,
  the app replaced by a small script after the window quits, then started again; on failure the
  old app comes back and starts with `--update-failed`. Automatic check at most once a day; a
  skipped version is not offered again by the automatic check.
- **Support pill and sheet** ("♥ Support"): the message and donation methods downloaded from
  `DeMoZ/donations` (`hubbridge.json`, schema 1) every time the sheet opens — raw.githubusercontent,
  then jsDelivr, then the last saved copy, then a copy built into the app; only `https://` links,
  opened in the browser; texts in the window's language.
- **Safe shutdown for an update**: running serial bridges are stopped and the phone disconnected
  before the app quits; the update is refused while the phone app is being installed or a
  pairing runs.
- Network through the system `curl` (macOS and Windows 10+), like the siblings: no TLS library
  in the app.
- **Tests** mirroring UsbMemoryChecker's: version parsing and order, release parsing per system,
  notes trimming, SHA-256, script quoting, donation parsing (languages, unsafe links, built-in
  file), settings defaults; plus the auto-check and offer rules and i18n key parity.

## Capabilities

### New Capabilities
- `app-updates`: how the desktop app learns about new releases and replaces itself.
- `donations`: the Support button, where its links come from and how they are shown.

### Modified Capabilities
<!-- none -->

## Impact

- New: `crates/desktop/src/net.rs`, `update.rs`, `donations.rs`, `crates/desktop/assets/donations.json`.
- Changed: `crates/desktop/src/app.rs` (settings fields, `Model.update` / `Model.donate`, new
  requests, timers, exit before an update), `crates/desktop/src/main.rs` (quit event,
  `--update-failed`), `crates/desktop/src/ui/index.html` (pills, two sheets, en/ru strings),
  `crates/desktop/Cargo.toml` (`sha2` from the workspace).
- External: a PR to `DeMoZ/donations` adding `hubbridge.json` (open question 1).
- Docs: `README.md`, `README.ru.md`, `docs/ROADMAP.md`, `docs/ARCHITECTURE.md`.
- No protocol, agent or Android changes.
