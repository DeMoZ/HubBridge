# Tasks

Spec: `specs/desktop-window-design/spec.md`. How: `design.md` (decision numbers below). Project rules: never declare a top-level `ipc` in the page script (the helper is `send()`);
the WebView lives on the main thread only; use `HUBBRIDGE_HOME=<scratch dir>` and
`--state-dir <scratch dir>` for every simulator / window run, and a non-default
`--listen 127.0.0.1:<port>` if 47800 is busy. Do not change any request JSON, `data-act`
value or `window.onState` / `window.onToast` signature. Window screenshots: get the window id
with a CGWindowList Swift snippet (owner name "HubBridge"), then
`screencapture -l <id> -o -x <scratch>/shot-<name>.png`; force a theme from
`HUBBRIDGE_DEV_SCRIPT` with `document.documentElement.dataset.theme='light'` (or `'dark'`).
Before starting, save the list of actions for the regression check:
`rg -o 'act: "[a-z_]+"|data-act|id="[A-Za-z]+"' crates/desktop/src/ui/index.html | sort -u > <scratch>/actions-before.txt`.

## 1. Guard tests first

- [x] 1.1 Add `#[cfg(test)] mod tests` to `crates/desktop/src/main.rs` with `page_declares_no_top_level_ipc` and `page_loads_nothing_remote` (design decision 8) against `include_str!("ui/index.html")`; verify both pass on the current page and that temporarily adding `const ipc = 1;` to the script makes the first one fail (revert the probe)
- [x] 1.2 Add `theme_tokens_meet_contrast` with a WCAG `contrast()` helper (unit-tested on black/white = 21.0 and `#777777`/white ≈ 4.48); it reads the dark `:root` block and both light blocks; verify it fails on the current page (light `--text-3` `#8a8fa5` on white is 3.3:1) — mark it `#[ignore]` until group 2 lands, then remove the ignore

## 2. Tokens, themes, canvas

- [x] 2.1 Replace the `:root` / light blocks with the token table of decision 2 (dark base, `@media (prefers-color-scheme: light) { :root:not([data-theme="dark"]) }`, `:root[data-theme="light"]`, `:root[data-theme="dark"]`), plus the type, spacing, radius, elevation and motion tokens of decisions 3 and 6; `body` background `--bg-0`, fonts from the tokens; verify `cargo test -p hubbridge-desktop` passes with `theme_tokens_meet_contrast` un-ignored
- [x] 2.2 Replace `.aurora` (markup and CSS) with the `body::before` ambient layer of decision 4; verify with the window (`cargo run -p hubbridge-desktop`, scratch `HUBBRIDGE_HOME`) and dark + light screenshots that the background shows two soft glows and no blobs, and that `rg -n "aurora|filter: blur" crates/desktop/src/ui/index.html` finds nothing
- [ ] 2.3 Add the preference overrides of decision 6b (reduced motion, more contrast, reduced transparency); verify by temporarily appending a dev script that adds a `<style>` copying the override rules unconditionally (WebKit cannot fake media queries), screenshot: panels opaque, no glow; then turn on System Settings → Accessibility → Display → Reduce motion and Increase contrast for one real run each and confirm the same by eye (restore the settings afterwards) — dropped to tech debt (owner decision 2026-11-28); tracked in docs/ROADMAP.md

## 3. Components

- [x] 3.1 Restyle `.btn` (primary, ghost, danger, small, disabled, busy spinner), `.chip` (neutral, ok, warn, err, info), `input.field`, `details > summary` chevron, `.note`, `.pill`, scrollbars, per decisions 5–6; verify a dev-script screenshot of a scratch "component sheet" (the script injects one of each into `#conn` temporarily) in both themes, and that hover/press/disabled look as specified
- [x] 3.2 Keyboard focus: `:focus-visible` ring for buttons, summaries and checkboxes, field focus ring, no ring on mouse click; give icon-only buttons an `aria-label` (sheet close, Copy) and `title` where the text is truncated; verify by tabbing through the left column in the running window (ring visible on every stop, order follows the layout) and clicking a button (no ring)
- [x] 3.3 Restyle `.modal` / `.sheet` (decision 5: scrim + blur, 420 px, radius 28, head row with optional icon tile and close button, actions row) and the PIN field; Escape and backdrop click close it, focus goes to the PIN field on open; add a shared `openSheet(id)` / `closeSheet(id)` pair and a document-level Escape handler that closes any open `.modal` (the updates change reuses them); verify in the window: open Pair, type 3 digits, press Escape → closed and nothing sent (no toast, no pairing in `HUBBRIDGE_LOG=debug` output); open again → focus in the PIN field
- [x] 3.4 Restyle toasts (decision 5: glass, radius 16, status dot + text, slide-in, same timeouts and click-to-dismiss); verify with `HUBBRIDGE_DEV_SCRIPT='window.onToast("ok","Paired with Pixel 8.");window.onToast("err","adb: device offline")'` screenshots in both themes

## 4. Shell and screens

- [x] 4.1 Top bar (decision 5): mark tile + name, centered link island (`#status`) with separate `put()` targets for the dot and the text, `#topActions` slot on the right holding `#ver` as faint text, drag on empty areas only (keep the mousedown rule; add `.top-actions` and `.pill` to its exclusion list); island text shows `Name · Wi-Fi · 12.4 ms` when connected; verify against the simulator (`hubbridge-agent-sim --pin 123456 --state-dir <scratch> --listen 127.0.0.1:<port>`, pair, connect by `127.0.0.1:<port>`): idle, connecting and connected island screenshots; dragging the empty bar moves the window, pressing the island does not
- [x] 4.2 Body grid and section headers (sentence case, count badges), independent column scrolling, minimum window 900×600; verify at the minimum size with the simulator's full device list (both columns visible, no horizontal scroll, each column scrolls)
- [x] 4.3 Agent and phone cards (`renderAgents`, `renderPhones`): card style, active card accent bar + border, chips, mono addresses, actions; verify screenshots with the simulator (paired online agent active, plus an unpaired one if a second simulator runs on another port) and with a fake adb model from a dev script calling `window.onState({...})` (phones with USB + Wi-Fi transports, `app_version` null and set, `wifi_refused` true) in both themes
- [x] 4.4 Connection panel (`renderConn`): connected hero with the 44 px RTT readout and the area sparkline (gradient stroke + fill from tokens), idle path with three joined step tiles, connecting / reconnecting / needs-pairing states; verify screenshots of each state in both themes (connected and idle from the simulator, the other two from dev-script models), and that the sparkline draws with 2 and with 90 samples
- [x] 4.5 USB devices (`renderDevices`, `serialRow`): device grid, icon tiles, Billboard dimming, details tree with guide line, serial row form and running state (PTY path, TCP, counters, Stop), error line; the focus-preserving rule for serial fields must keep working; verify with the simulator: expand Details on 1002, start a serial bridge with PTY on 1002 and type in the baud field while counters update (focus not lost), screenshot running and stopped-with-reason states in both themes
- [x] 4.6 Regression check of actions: run the `rg` command from the header again into `<scratch>/actions-after.txt` and `diff` with the before list; every `act:` value and every id the script uses is still present; then click through every action of the spec scenario "Every action still there" against the simulator (adb actions with a phone if one is attached, otherwise check that the buttons render from a fake model and send the right JSON by temporarily wrapping `send` in a dev script that logs to a toast)

## 5. Mark and icons

- [x] 5.1 Inline SVG mark in the top bar per decision 7; verify it renders crisp at 28 px in both themes (screenshot zoomed)
- [x] 5.2 Add `scripts/make-icon.swift` (draws `crates/desktop/assets/icon.png`, 1024×1024, decision 7) and `scripts/make-icon.sh` (iconset 16…512 @1x/@2x with `sips`, `iconutil -c icns` → `crates/desktop/assets/AppIcon.icns`), modelled on `/Users/roman/Git/UsbMemoryChecker/scripts/make-icon.sh` without the `.ico` part; commit both outputs; verify `scripts/make-icon.sh` runs on this Mac, `sips -g pixelWidth crates/desktop/assets/icon.png` reports 1024, and `iconutil -c iconset crates/desktop/assets/AppIcon.icns -o <scratch>/x.iconset` lists 10 images
- [x] 5.3 Redraw the Android launcher icon as an adaptive icon (`mipmap-anydpi-v26/ic_launcher.xml`, layers in `drawable/ic_launcher_{background,foreground,monochrome}.xml`) with the same mark (decision 7); verify `cd android && ./gradlew assembleDebug` succeeds (`ANDROID_HOME=/opt/homebrew/share/android-commandlinetools`, `JAVA_HOME=$(/usr/libexec/java_home -v 25)`) and, if a phone is attached, the launcher shows the new icon after `hubbridge install-apk`

## 6. Docs

- [x] 6.1 `docs/ARCHITECTURE.md`: a "Desktop window" section (tokens live in the page's `:root` blocks as hex literals, light and opaque overrides written twice and test-guarded, `data-theme` hook for screenshots, the guard tests, `openSheet`/`closeSheet`, icon script) and the repository layout (`crates/desktop/assets/`, `scripts/make-icon.*`); WebKit does not support `prefers-reduced-transparency` (design.md); verify with `rg -n "data-theme" docs/ARCHITECTURE.md` and `rg -n "make-icon" docs/ARCHITECTURE.md`. CLAUDE.md is not changed (agent notes live elsewhere)
- [x] 6.2 `docs/ROADMAP.md` desktop window: add a checked line for the redesign (Ambient Glass, AA contrast, reduced motion) and an open item "Android control screen in the desktop's visual language"; `README.md` and `README.ru.md`: add a window screenshot (`docs/screenshot-desktop.png`, dark theme, simulator connected) under "Desktop app"; verify the image path resolves (`test -f docs/screenshot-desktop.png`) and both READMEs reference it

## 7. Integration checks

- [x] 7.1 Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`; verify all pass on macOS
- [ ] 7.2 Run `cargo check --target x86_64-pc-windows-msvc -p hubbridge-desktop`; verify it passes or fails only in the known `ring` C build (then leave it to CI), and after push the macOS and Windows CI jobs are green
- [ ] 7.3 Full visual pass against the simulator in both themes (forced with `data-theme`) and in the real system appearance once each: idle, connecting, connected with devices and a running serial bridge, needs pairing, pairing sheet, toasts; collect the screenshots in `<scratch>` and attach the dark/light connected pair to the PR description; verify the spec scenarios "System switches to light while the window is open" (switch the macOS appearance with the Pair sheet open and 3 digits typed) and "No network" (Wi-Fi off, window looks the same)

## Workflow follow-up

- Review (fresh Opus subagent) against `specs/desktop-window-design/spec.md` and this design.
- After review is clean: `/opsx:archive desktop-redesign-2035`, then continue with `release-pipeline`.
