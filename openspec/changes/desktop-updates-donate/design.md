# Design

## Context

- Reference implementation: UsbMemoryChecker (`/Users/roman/Git/UsbMemoryChecker`), same stack
  (tao 0.37 + wry 0.57). Its `src/update.rs`, `src/donations.rs`, `src/net.rs` (taken from
  360Cleaner) and the update/donate parts of `src/main.rs` and `src/ui/index.html` are the
  model for everything below: endpoints, file names, texts, intervals, button set, tests. Read
  them first; this design lists only what differs.
- HubBridge's controller (`crates/desktop/src/app.rs`) differs from UsbMemoryChecker's: it runs
  on a tokio runtime, the page gets the whole `Model` through `window.onState` after every
  change and toasts through `window.onToast`, and requests are a serde-tagged `Request` enum
  (`{"action": "snake_case", …}`), not a string switch. Settings live in `desktop.json` in
  `hubbridge_host::store::default_dir()` (which honors `HUBBRIDGE_HOME`).
- The window strings are an inline `I18N` object (en, ru) in `index.html`; the language follows
  `navigator.language` in the page, Rust does not know it.
- `release-pipeline` publishes `HubBridge-X.Y.Z-macos-arm64.zip` (contains `HubBridge.app`,
  zipped with `ditto -c -k --keepParent`, ad-hoc signed) for this updater; no Windows file.
- `desktop-redesign-2035` provides `.pill`, `.modal`/`.sheet`, `openSheet`/`closeSheet`, the
  Escape handler, the `#topActions` slot and the `--support` token.
- `main.rs` has `UserEvent::{Js, DragWindow}`; there is no way for the controller to quit the
  event loop yet.

## Goals / Non-Goals

**Goals:**
- Same behavior, texts and tests as UsbMemoryChecker's updater and Support window, adapted to
  HubBridge's model/request architecture.
- An update never leaves a serial bridge's port half-open or the phone mid-install.

**Non-Goals:**
- Updating the phone app from the desktop when versions differ (the bundled APK of the new
  desktop version is installed by the existing "Install app" button; an automatic
  "phone app is older" prompt is a later change).
- Sparkle, delta updates, a background downloader, rollback to an older version.
- More languages, a language menu or a settings page (ROADMAP keeps them).
- Windows release files (see proposal, open question 4).

## Decisions

### 1. Three modules copied from UsbMemoryChecker, comments in English

`crates/desktop/src/net.rs`, `update.rs`, `donations.rs` start as copies of UsbMemoryChecker's
files with the comments translated to English (HubBridge's code is commented in English) and
these changes:

- `net.rs`: user agent `HubBridge`; `config_dir()` / `cache_dir()` replaced by one
  `fn data_dir() -> PathBuf` = `hubbridge_host::store::default_dir()` (so `HUBBRIDGE_HOME`
  isolates checks); `output_timeout` is not needed (drop it and its tests). Keep `command()`
  (no console window on Windows), `get`, `download`, `open_url`, `is_https` and the
  `only_https_links` test as is. Drop the Linux `xdg-open` branch (no Linux platform).
- `update.rs`: `REPO = "DeMoZ/HubBridge"`, env override `HUBBRIDGE_UPDATES_URL`, work prefix
  `HubBridge-update-`, `EXE = "HubBridge.exe"`, script names `HubBridge-update.sh/.ps1`,
  probe file `.hubbridge-update-check-<pid>`. Differences in behavior: decisions 2–3.
- `donations.rs`: `FILE = "hubbridge.json"`, built-in copy `crates/desktop/assets/donations.json`,
  cache file `<data_dir>/donations.json`. Parsing, language choice, sources and order unchanged.

Network calls are blocking (`curl` child processes); the controller runs them with
`tokio::task::spawn_blocking`.

Alternative: an HTTP client crate (reqwest/ureq + rustls) — rejected: the siblings use curl to
keep TLS out of the binary; macOS and Windows 10+ ship curl.

### 2. Release parsing keeps "newer but no file for this system"

`parse_release(json, suffix) -> Option<Release>` returns `None` only for a malformed answer.
`Release { version, file: Option<Asset { url, sha256: Option<String> }>, page_url, notes }`.
The controller then reports:

| Found | `file` | Status |
|---|---|---|
| version ≤ current | – | `updLatest` |
| newer | `Some` | `updNew` + offer rule (decision 5) |
| newer | `None` | `updNoFile` (kind `new`, sheet shows "Open release page" only) |

UsbMemoryChecker's `parse_release` returns `None` when the asset is missing, which shows "up
to date" on a system without a release file — misleading on Windows here.

### 3. Stricter install checks

- **Digest required.** `download()` fails when `sha256` is `None` ("the release does not
  publish a SHA-256 for this file"). UsbMemoryChecker accepts a missing digest because its
  oldest releases predate GitHub's digests; HubBridge has no such releases.
- **Right app inside.** macOS: look for a directory named exactly `HubBridge.app` (not any
  `.app`), then read its id with
  `/usr/bin/plutil -extract CFBundleIdentifier raw -o - <app>/Contents/Info.plist` and require
  `com.demoz.hubbridge`. Windows: a file named `HubBridge.exe`. Otherwise
  "the update archive has no HubBridge app inside".
- `install_target()`, `can_write()`, `mac_script()`, `windows_script()`, `start_install()` as
  in UsbMemoryChecker (the macOS script waits for the pid, swaps `HubBridge.app` via
  `HubBridge.app.old`, removes quarantine, `open`s the new app, or restores the old one and
  opens it with `--args --update-failed`).

### 4. Settings fields

`Settings` in `app.rs` gains (serde `rename_all` is not used in this file; keep snake_case):

```rust
#[serde(default = "yes")] auto_update: bool,      // default true
#[serde(default)] last_update_check: u64,          // Unix seconds
#[serde(default)] skipped_version: String,
```

`Settings` derives `Default`, which would make `auto_update` false for a fresh install: replace
the derive with a manual `impl Default` that sets `auto_update: true`, and test both a missing
file (`Settings::default()`) and an old `desktop.json` without the new fields.

### 5. Controller state, model and requests

```rust
// Model (pushed to the page)
pub update: UpdateView,   // can_update, writable, auto_update, checking, downloading,
                          // available: Option<{version, notes, page}>, no_file: bool,
                          // status: Option<{kind, key, args}>, offer: u64, failed_launch: bool
pub donate: DonateView,   // loading, offline, info: Option<donations::Info>
```

- `offer` is a counter: the page opens the update sheet when it sees the value grow (one
  channel for everything, no extra `window.on*` callback). `failed_launch` is true when the
  process got `--update-failed`; the page shows the "didn't install" toast once.
- `status.key` / `args` are I18N keys and parameters translated by the page, as in
  UsbMemoryChecker (`updNew`, `updLatest`, `updNoFile`, `updCheckFailed`, `updDownloading`,
  `updRestarting`, `updFailed`, `updWaitBusy`).
- New `Request` variants: `UpdateCheck`, `UpdateInstall`, `UpdateSkip`, `UpdatePage`,
  `SetAutoUpdate { value: bool }`, `DonateOpen { lang: String }`, `OpenUrl { url: String }`.
  `DonateOpen` carries the page's language (Rust stores it and parses the cached bytes with it,
  so a language change needs no Rust-side detection).
- Pure helpers with unit tests:
  `fn should_auto_check(s: &Settings, can_update: bool, now: u64) -> bool`
  (`s.auto_update && can_update && now.saturating_sub(s.last_update_check) >= 86_400`) and
  `fn should_offer(manual: bool, skipped: &str, found: &str) -> bool` (`manual || skipped != found`).
- Timers: on the first `Ready` and then every hour (a tokio interval started in
  `start_background`), `if should_auto_check(..) { check(manual = false) }`.
  `last_update_check` is saved only after a successful answer (as UsbMemoryChecker), so an
  offline start retries at the next hourly tick.
- `UpdateSkip` saves `skipped_version`; `UpdatePage` opens the release page or
  `https://github.com/DeMoZ/HubBridge/releases`; `SetAutoUpdate` saves the flag.

### 6. Installing: busy rule, clean exit

- `UpdateInstall` is refused with `updWaitBusy` while `Model.busy` has an `install:*` entry or
  `pair` (an APK install or a pairing is talking to the phone); spec "Update refused while the
  phone is busy".
- Order: download + verify + unpack (window keeps working, status `updDownloading`) →
  `stop_all_bridges("the app is updating")` → `disconnect()` → `start_install(source, target)` →
  status `updRestarting` → call the exit hook. If `start_install` fails, bridges stay stopped
  (the user can start them again) and the status is `updFailed`.
- Exit hook: `App::new(rt, push, exit)` with `pub type Exit = Arc<dyn Fn() + Send + Sync>`;
  `main.rs` adds `UserEvent::Quit` and sets `ControlFlow::Exit` on it. The replace script waits
  for the process id to disappear, so it must really exit (not just hide the window).
- The page shows `updBridges` ("Running serial bridges will stop.") in the sheet when
  `M.serial` has a running bridge and an update is available.

### 7. Page

All texts below are UsbMemoryChecker's `i18n.js` en/ru strings with the product name changed,
plus three new keys: `updNoFile` ("Version {v} is out, but it has no download for this
system."), `updWaitBusy` ("Wait until the phone action finishes, then update.") and `updBridges`
("Running serial bridges will stop."), each with a Russian text.

- Top bar `#topActions` (from the redesign): version pill `#updBtn` (`.pill`, dot, up-arrow
  icon, label `v0.1.0` / `versionBtn`), Support pill (`.pill support`, heart icon, label
  "Support" / "Поддержать", hover color `--support`). `#ver` from the redesign is replaced by the
  version pill.
- Update sheet `#updateModal` and Support sheet `#donateModal` use `.modal`/`.sheet` and
  `openSheet`/`closeSheet`; their markup follows UsbMemoryChecker's (`updTitle`, `updCurrent`,
  `updNotes` pre-wrap block max-height 240 px with its own scroll, `updWarn`, `updStatus`, the
  `autoUpdate` checkbox, `updDev`, `updActions`; `donateMessage`, `donateMethods`,
  `donateOffline`, spinner). Method rows: emoji by id (`cloudtips` 💳, `tribute` 🌍, else 🤝),
  title, description, ↗; whole row is a button → `send({action: "open_url", url})`.
- `window.onState` gains `renderUpdate()` / `renderDonate()`; a `lastOffer` variable opens the
  update sheet when `M.update.offer > lastOffer`; `failed_launch` toasts once.
- New keys go into both `en` and `ru`; a Rust test (`i18n_keys_match`) extracts the key names
  of the two blocks from `include_str!("ui/index.html")` and asserts the sets are equal.

### 8. Donations file in DeMoZ/donations

`crates/desktop/assets/donations.json` (built-in) and `hubbridge.json` in DeMoZ/donations have
the same content (a rule in docs/ARCHITECTURE.md, like UsbMemoryChecker's "keep identical"). Content: schema 1,
`message` en/ru (proposal, open question 2), the two methods with the same titles, descriptions
and links as `universalkm.json` in that repo. The PR there adds the file and a README table
row `| hubbridge.json | HubBridge |`.

## Risks / Trade-offs

- [GitHub API rate limit for unauthenticated calls (60/hour per IP)] → at most one automatic
  call per day plus manual checks; the error is shown as `updCheckFailed`.
- [The replace script runs after the app quits; if the user starts the old app in between, the
  move fails] → the script restores the old app and starts it with `--update-failed`
  (UsbMemoryChecker behavior).
- [Ad-hoc signed new app may ask for Local Network access again after an update] → documented by
  `release-pipeline` (LIMITATIONS); not something the updater can fix.
- [The PR to DeMoZ/donations is not merged at release time] → the built-in copy is shown with
  the "saved copy" note; nothing breaks.
- [A long-running window keeps an old `Settings` copy while another instance writes
  `desktop.json`] → same as today for the other settings; one window per user is the normal case.

## Migration Plan

- Old `desktop.json` files load with the new fields at their defaults (auto update on).
- The first release containing the updater is v0.1.0; it can only be tested end to end when
  v0.1.1 exists (tasks, workflow follow-up). Rollback: a release that breaks updating is fixed by
  publishing a newer release users download by hand.
