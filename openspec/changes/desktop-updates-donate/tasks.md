# Tasks

Specs: `specs/app-updates/spec.md`, `specs/donations/spec.md`. How: `design.md` (decision
numbers below). Reference (read only, never modify): `/Users/roman/Git/UsbMemoryChecker/src/update.rs`,
`src/donations.rs`, `src/net.rs`, `src/settings.rs`, `src/main.rs` (functions `init_msg`,
`handle_request`, `open_donations`, `update_msg`, `check_updates`, `install_update`),
`src/ui/index.html` (update/donate markup and `renderUpdate` / `renderDonations`),
`src/ui/i18n.js` (en/ru strings), `assets/donations.json`. Project rules: never declare a
top-level `ipc` in the page (helper `send()`), the WebView lives on the main thread only, use
`HUBBRIDGE_HOME=<scratch dir>` for every window run. Dev-script model overrides: the page's
top-level `let M` is reachable from `HUBBRIDGE_DEV_SCRIPT`, e.g.
`window.onState({...M, update: {...M.update, available: {version: "0.2.0", notes: "What's new\n- x", page: "https://github.com/DeMoZ/HubBridge/releases/tag/v0.2.0"}, offer: 1}})`.
Window screenshots: CGWindowList id (owner name prefix "HubBridge"), `screencapture -l <id> -o -x`). Do this
change after `desktop-redesign-2035` (components) and `release-pipeline` (packaged app for the
checks in 7.3).

## 1. Network helpers (`crates/desktop/src/net.rs`)

- [x] 1.1 Copy UsbMemoryChecker's `net.rs` with the changes of design decision 1 (English comments, user agent `HubBridge`, `data_dir()` from `hubbridge_host::store::default_dir()`, no `output_timeout`, no Linux branch); `mod net;` in `main.rs`; verify `cargo test -p hubbridge-desktop` runs `only_https_links` green and `cargo clippy -p hubbridge-desktop --all-targets -- -D warnings` is clean (`#[allow(dead_code)]` only where a function is used by a later group, removed when that group lands)
  - Result: `only_https_links` green (plus a non-ASCII case: the length check no longer slices inside a character), clippy `-D warnings` clean, no `allow(dead_code)` left.

## 2. Settings

- [x] 2.1 Add `auto_update` (default true), `last_update_check`, `skipped_version` to `Settings` in `app.rs` with a manual `impl Default` (design decision 4); verify new unit tests: `Settings::default().auto_update` is true; `serde_json::from_str::<Settings>(r#"{"adb_wifi":{},"last_target":null}"#)` (an old file) gives `auto_update == true`, `last_update_check == 0`, empty `skipped_version`; a round trip keeps all fields
  - Result: `settings_defaults_old_files_and_round_trip` passes (default, old file, `{}`, round trip).
- [x] 2.2 Add `should_auto_check` and `should_offer` (decision 5) as free functions; verify unit tests: off → false; dev build (`can_update == false`) → false; 25 h since last → true; 3 h → false; `last_update_check` in the future (clock moved back) → false; `should_offer(false, "0.2.0", "0.2.0") == false`, `(true, "0.2.0", "0.2.0") == true`, `(false, "0.2.0", "0.3.0") == true`
  - Result: `auto_check_once_a_day` and `skipped_version_is_not_offered_again` pass, with every listed case.

## 3. Updater (`crates/desktop/src/update.rs`)

- [x] 3.1 Copy UsbMemoryChecker's `update.rs` with the renames of decision 1 and add `sha2 = { workspace = true }` to `crates/desktop/Cargo.toml`; port its tests (`release_for_each_system`, `notes_without_install_sections`, `versions`, `sha256_of_file`, `scripts_quote_paths`) with HubBridge names (`HubBridge-0.4.0-macos-arm64.zip`, `/Applications/HubBridge.app`, `HubBridge.exe`); verify they pass
  - Result: the five ported tests pass; `sha256_of_file` also covers the match and mismatch of the digest check.
- [x] 3.2 Change `parse_release` to keep a newer release without a file for this system (`file: Option<Asset>`, decision 2); verify tests: the macOS suffix finds the zip with its lowercased digest; the Windows suffix on the same JSON gives `Some(release)` with `file: None` and version 0.4.0; `"{}"` and a JSON without `tag_name` give `None`; a tag `v0.10.0` compares newer than `0.9.9`
  - Result: `release_for_each_system` and `versions` pass; the table of decision 2 is covered by `check_results_reach_the_page`.
- [x] 3.3 Stricter `download` / unpack (decision 3): refuse a missing digest; split unpacking into `fn unpack(zip: &Path, work: &Path) -> Result<PathBuf, String>` so it can be tested without the network; find exactly `HubBridge.app` and check `CFBundleIdentifier == com.demoz.hubbridge` with `plutil` (macOS) or `HubBridge.exe` (Windows); verify `#[cfg(target_os = "macos")]` tests that build fake bundles in a temp dir and zip them with `ditto -c -k --keepParent`: a `HubBridge.app` with the right id unpacks and is returned; `Other.app` → "has no HubBridge app inside"; `HubBridge.app` with id `com.example.x` → same error; and a unit test that `download` with `sha256: None` fails before any network call (use an `https://invalid.invalid/` URL so a regression would fail fast)
  - Result: `unpack_returns_the_hubbridge_app`, `unpack_refuses_another_app_name`, `unpack_refuses_another_bundle_id` and `download_needs_a_digest` pass on macOS. The Windows branch is only type-checked.
- [x] 3.4 `#[cfg(target_os = "macos")]` test `mac_script_swaps_the_app`: temp dirs `<tmp>/HubBridge-update-0.2.0/app/HubBridge.app` (marker file `new`) and `<tmp>/Apps/HubBridge.app` (marker `old`), script from `mac_script(source, target, pid)` with a pid that is not running (spawn and wait `/usr/bin/true`, use its pid) and every `open ` replaced by `: ` before running it with `/bin/bash`; verify the target has `new`, `HubBridge.app.old` and the work dir are gone
  - Result: `mac_script_swaps_the_app` passes; added `mac_script_restores_the_old_app_when_the_swap_fails`, which passes too.

## 4. Donations (`crates/desktop/src/donations.rs`, `crates/desktop/assets/donations.json`)

- [x] 4.1 Write `assets/donations.json` (schema 1; `message` en/ru from proposal open question 2; methods `tribute` and `cloudtips` with the titles, descriptions and URLs of `universalkm.json` in DeMoZ/donations — read it with `gh api repos/DeMoZ/donations/contents/universalkm.json --jq .content | base64 -d`); verify it parses with `python3 -I -m json.tool` and has `en` and `ru` for every text
  - Result: `python3 -I -m json.tool` parses it; the methods are copied from `universalkm.json` (read with `gh api`); `built_in_file_is_valid` checks `en` and `ru` for every text.
- [x] 4.2 Copy UsbMemoryChecker's `donations.rs` with `FILE = "hubbridge.json"`, cache at `<data_dir>/donations.json` (decision 1); port the tests: `built_in_file_is_valid` (for `en` and `ru`: message mentions HubBridge, every method has title and description in both, all links https), `languages_and_unsafe_links`, `base_language`; add `download_keeps_cache_on_bad_data` by testing the "parse before save" rule on bytes (`parse(b"not json", "en").is_none()`, and `cached()` falls back to the built-in copy when the cache file holds garbage — use `HUBBRIDGE_HOME` set to a temp dir inside the test, serialized with a static mutex since env is process-wide); verify all pass
  - Result: `built_in_file_is_valid`, `languages_and_unsafe_links`, `base_language` and `download_keeps_cache_on_bad_data` pass; the last one drives the real download loop with a fake fetch.

## 5. Controller and window wiring (`app.rs`, `main.rs`)

- [x] 5.1 `Model.update: UpdateView` and `Model.donate: DonateView` (decision 5), new `Request` variants; verify a unit test deserializes `{"action":"update_check"}`, `{"action":"update_install"}`, `{"action":"update_skip"}`, `{"action":"update_page"}`, `{"action":"set_auto_update","value":false}`, `{"action":"donate_open","lang":"ru"}`, `{"action":"open_url","url":"https://x"}`, and that `serde_json::to_value(Model::default())` has `update.offer == 0` and `donate.loading == false`
  - Result: `update_and_donate_requests_deserialize` passes.
- [x] 5.2 Update check flow: `check(manual)` in `spawn_blocking`, statuses per decision 2 table, `last_update_check` saved on success only, `offer` incremented per `should_offer`, first-`Ready` + hourly auto check per `should_auto_check`, `UpdateSkip` / `UpdatePage` / `SetAutoUpdate`; `can_update` / `writable` from `update::install_target` / `can_write`; verify with a release build run from a scratch copy of the packaged app (`scripts/build-macos.sh`, then `open --env HUBBRIDGE_HOME=<scratch> --env HUBBRIDGE_UPDATES_URL=https://api.github.com/repos/DeMoZ/UsbMemoryChecker/releases/latest <copy>/HubBridge.app`): the pill reads "v0.1.2 is out" and the sheet opens by itself; with `HUBBRIDGE_UPDATES_URL=https://api.github.com/repos/DeMoZ/HubBridge/releases/latest` before any release → "Couldn't check for updates:" (404); `desktop.json` in the scratch dir has `last_update_check` set only after the successful check
  - Result: packaged app (`scripts/build-macos.sh --skip-tests` with a failing `hdiutil` shim: app and zip built, no `.dmg`), scratch copy, scratch `HUBBRIDGE_HOME`, fake `ADB`; read from the live window's DOM and model. UsbMemoryChecker URL: pill "v0.1.3 is out" (its latest release is now 0.1.3, not 0.1.2), sheet opened by itself, `last_update_check` saved. HubBridge URL: "Couldn't check for updates: curl: (56) The requested URL returned error: 404", no `desktop.json` written. Also: checked 3 h ago, no check; Skip saves 0.1.3 and the next automatic check shows the pill without the sheet; setting off stays off after a restart with an old check; offline (`HTTPS_PROXY` to a closed port, not Wi-Fi off) shows curl's reason. The hourly timer was not waited for.
- [x] 5.3 Install flow (decision 6): busy refusal `updWaitBusy`, download/verify/unpack, stop bridges, disconnect, `start_install`, exit hook (`App::new(rt, push, exit)`, `UserEvent::Quit` in `main.rs`), `--update-failed` → `failed_launch`; verify (a) in the scratch packaged app with the UsbMemoryChecker URL from 5.2, "Update and restart" downloads, the digest matches, and the update is refused with "the update archive has no HubBridge app inside" while the app keeps running (proves download + SHA-256 + identity guard against a real GitHub release); (b) a unit test of the busy rule (`fn update_blocked(busy: &BTreeSet<String>) -> bool`: `install:R28M` → true, `pair` → true, `wifi:R28M` → false, empty → false); (c) starting the packaged app with `--args --update-failed` shows the "didn't install" toast once
  - Result: (a) UsbMemoryChecker update downloaded, digest matched, refused with "the update archive has no HubBridge app inside", app kept running. (b) `update_waits_for_the_phone` passes; in the window, a running pairing gave `updWaitBusy` with no download. (c) `--args --update-failed`: one toast, still one after a second push. End to end with a local HTTPS server (`CURL_CA_BUNDLE`), a fake 0.2.0 bundle and the agent simulator: the serial bridge's TCP port closed while the old process was alive, then it exited, the bundle in the same folder became 0.2.0 and started by itself, the download folder was removed; with the folder made read-only before the swap, the old app stayed, started again and showed the toast once; a wrong digest was refused and nothing replaced; a read-only folder offered "Open release page". `/Applications` was not touched.
- [x] 5.4 Donations flow: `DonateOpen { lang }` shows the cached/built-in info with `loading: true` at once, downloads in `spawn_blocking` (raw, then jsDelivr), saves a valid file, then `loading: false` and `offline` per result; a second open while loading only re-pushes; `OpenUrl` → `net::open_url` (https only, error toast otherwise); verify with the window: Support sheet with network (spinner, then the list; before the DeMoZ/donations PR is merged the note "showing a saved copy" is expected), with Wi-Fi off (built-in copy + note), and `send({action:"open_url",url:"http://x"})` from a dev script gives an error toast and opens nothing
  - Result: in the packaged window (live DOM): spinner with the built-in list, then no spinner, the list and the "saved copy" note (both sources answer 404 until the DeMoZ/donations PR is merged), nothing cached; `open_url` with `http://x` gave the error toast. Offline was simulated with `HTTPS_PROXY` to a closed port; **Wi-Fi off not verified** (not toggled). A successful fresh download is covered only by the unit test.

## 6. Page (`crates/desktop/src/ui/index.html`)

- [x] 6.1 Version pill and Support pill in `#topActions` (decision 7), both excluded from the drag rule; update sheet and Support sheet markup and `renderUpdate()` / `renderDonate()` ported from UsbMemoryChecker; `lastOffer` handling; `failed_launch` toast; Serial-bridges warning; en + ru strings (UsbMemoryChecker's texts with HubBridge's name + the three new keys); verify screenshots in both themes (`data-theme`) from dev-script models: no update (`v0.1.0`), newer available with notes, downloading (spinner), check failed, no file for this system, dev build note, read-only warning, bridges warning; Support sheet loading, loaded, offline
  - Result: all listed states rendered in dark and light and reviewed, plus Russian at 900x600 with long notes (240 px block scrolls, scroll kept across pushes). These are snapshots of the real `index.html` in an off-screen WKWebView with dev-script models: **screenshots of the app window itself not verified** (screen locked, `screencapture -l` fails).
- [x] 6.2 Add the Rust test `i18n_keys_match` (decision 7) in `main.rs` tests; verify it passes, and fails when a key is temporarily removed from `ru` (revert the probe)
  - Result: `i18n_keys_match` passes; with `updBridges` removed from `ru` it failed (`missing in ru: ["updBridges"]`); probe reverted.
- [ ] 6.3 Keyboard and sheet behavior: Escape closes either sheet, the backdrop closes it, Tab reaches every button and the checkbox with a visible ring, the method rows are buttons (Enter opens the link); verify by hand in the window
  - Result: **not verified by hand** (screen locked). Checked by script: in a dev build, synthetic keydown events: Escape and a backdrop mousedown close both sheets and focus returns to the pill; the sheet's Tab handler cycles through close, checkbox and every enabled button; method rows are `<button type="button">` and a click sends `open_url` with their link; focus survives re-renders (same control, else the sheet heading). The 2 px focus ring on every new control was computed in the off-screen WKWebView in both themes. Open: real key presses, including Enter on a method row and Space on the checkbox.
- [x] 6.4 Re-run the redesign's guard tests (`page_declares_no_top_level_ipc`, `page_loads_nothing_remote`, `theme_tokens_meet_contrast`); verify they still pass — the donation URLs are data from Rust, not `src`/`href` attributes in the page source
  - Result: `page_declares_no_top_level_ipc`, `page_loads_nothing_remote`, `theme_tokens_meet_contrast` pass. `every_put_host_has_a_focusable_fallback` was extended to sheet hosts (`h3` with `tabindex=-1`).

## 7. Donations repository and docs

- [ ] 7.1 Only if Roman confirmed proposal open question 1: clone `DeMoZ/donations` into the scratch dir, branch `add-hubbridge`, add `hubbridge.json` (byte-identical to `crates/desktop/assets/donations.json`) and the README row, commit, push the branch, `gh pr create` (English title/body); verify `cmp` of the two files succeeds and the PR URL is in the report; otherwise list it as an open item in the report
  - Result: not the executor's task; no PR opened.
- [x] 7.2 `README.md` / `README.ru.md` "Desktop app": updates (automatic once a day, Skip, works in the release app only) and the Support button; `docs/ARCHITECTURE.md` "Desktop window" / developer notes (the repo has no CLAUDE.md): modules `net.rs` / `update.rs` / `donations.rs`, `HUBBRIDGE_UPDATES_URL`, keep `assets/donations.json` and `hubbridge.json` in DeMoZ/donations identical, release notes are shown in the update window (Install sections hidden), the `offer` counter pattern; `docs/ARCHITECTURE.md` repository layout (desktop modules, assets); `docs/ROADMAP.md` Step 4: tick "Updates and Support button like the other DeMoZ apps"; verify `rg -n "HUBBRIDGE_UPDATES_URL" docs/ARCHITECTURE.md` and `rg -n "Support" docs/ROADMAP.md README.md README.ru.md` find the new text
  - Result: both `rg` commands find the new text. `docs/releases/v0.1.0.md` already matches what was built; unchanged.

## 8. Integration checks

- [x] 8.1 Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`; verify all pass on macOS
  - Result: fmt clean, clippy `-D warnings` clean, `cargo test --workspace`: 211 passed, 0 failed.
- [ ] 8.2 Run `cargo check --target x86_64-pc-windows-msvc -p hubbridge-desktop`; verify it passes or fails only in the known `ring` C build (leave to CI); after push the Windows CI job compiles and tests `update.rs` (Windows script test included) and is green
  - Result: local part done: `cargo check --target x86_64-pc-windows-msvc -p hubbridge-desktop` fails only in the `ring` C build; `net.rs`, `update.rs` and `donations.rs` also pass `cargo clippy --target x86_64-pc-windows-msvc --all-targets` without errors in a scratch crate without `ring`. **The Windows CI job is not verified** (nothing pushed).
- [x] 8.3 Regression: with the simulator connected and a serial bridge running in a dev build, open and close both sheets, press "Check for updates" (disabled in a dev build) and confirm the bridge counters keep growing and no request reaches Rust except `donate_open` / `open_url` (watch `HUBBRIDGE_LOG=debug`)
  - Result: dev build, agent simulator, serial bridge with traffic: counters grew from 384 to 10944 bytes while both sheets were opened and closed; "Check for updates" disabled; after the bridge started only `donate_open` and `open_url` reached Rust. `HUBBRIDGE_LOG=debug` does not log page requests, so a temporary probe printed them (and replaced the browser launch with a print); reverted.

## 9. Review fixes

- [x] 9.1 Status dot: the hiding rule names `#updBtn`; test `no_rule_hides_the_status_dot`
  - Result: the test fails with the old `.pill .dot` rule. Dev build window (scratch `HUBBRIDGE_HOME`, fake `ADB`): `#stMark .dot` is `display: block`, 8 px (11.3 px box for the rotated error dot) in idle, connecting, connected, reconnecting and needs-pair models; the version pill's dot is hidden without an update and 8 px with one; a window screenshot shows the idle ring.
- [x] 9.2 Windows script: UTF-8 BOM, `ps_quote` doubles U+2018, U+2019, U+201A, U+201B
  - Result: `windows_script_file_has_a_byte_order_mark` and `scripts_quote_paths` pass on macOS; `net.rs` and `update.rs` pass clippy for `x86_64-pc-windows-msvc` in a scratch crate without `ring`. PowerShell 7 (`pwsh` on macOS) parses the generated script without errors and reads back a BOM script's path with Cyrillic and all five quote characters unchanged. **Not run in Windows PowerShell 5.1.**
- [x] 9.3 Shutdown phase: no new APK install, pairing or bridge start; bridges still starting are waited for; the flag is cleared when the install stops
  - Result: `nothing_new_starts_once_the_app_closes_for_an_update` and `shutdown_waits_for_starting_bridges` pass. **The timing gap was not reproduced end to end.**
- [x] 9.4 The download folder is removed when the update is not installed, also by the failed swap in the replace scripts
  - Result: `a_failed_download_leaves_no_folder`, `discard_removes_only_a_download_folder`, `mac_script_restores_the_old_app_when_the_swap_fails` (extended) and `scripts_quote_paths` (Windows order) pass; the `discard` calls in `update_install` are checked by reading only.
- [x] 9.5 `update_install` guard; the update sheet waits for the Support sheet; the auto-update checkbox is written only when the setting changes; docs and spec delta
  - Result: `only_an_installable_update_is_installed` and `the_update_sheet_does_not_open_over_another_sheet` pass (the latter fails on the old code). In the window: a stale push after a click left the box unticked; with Support open an offer did not open the update sheet, and it opened after Support closed, focus on its close button. `openspec validate desktop-updates-donate --strict` passes.

- [x] 9.6 macOS script: a partial app left by a failed move across volumes is removed and the old one restored, only after the old app was really moved aside; the download folder is removed after the restore
  - Result: `mac_script_removes_a_partly_moved_app` (the move of the new app replaced by one that leaves a partial folder and fails) and `mac_script_keeps_the_app_it_could_not_move_aside` (move aside fails; a leftover `.app.old` that was not removed) pass with the two older script tests; both new tests fail with the previous script. **Not run with a real second volume.** The Windows script has the same class of problem and is unchanged: `robocopy /E` copies over the installed folder with no backup, so a failed copy can leave a mix of old and new files, which it then starts with `--update-failed`; no Windows file is released.
- After the fixes: `cargo fmt --all`, clippy `-D warnings` clean, `cargo test --workspace`: 221 passed, 0 failed.

## Workflow follow-up

- Review (fresh Opus subagent) against both specs and this design.
- After review is clean: `/opsx:archive desktop-updates-donate`; release v0.1.0 per
  `release-pipeline`'s follow-up.
- After v0.1.0 is published: the installed v0.1.0 (from the real `.dmg`, in `/Applications`)
  reports "You have the latest version, 0.1.0."
- At v0.1.1: from v0.1.0 in `/Applications`, "Update and restart" replaces the app and v0.1.1
  starts by itself (spec "Successful update"); check whether macOS asked for Local Network
  access again and record it in `docs/LIMITATIONS.md`.
