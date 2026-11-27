# Design

## Context

- The window is one page, `crates/desktop/src/ui/index.html` (619 lines: CSS, markup, en/ru
  strings, render functions), compiled in with `include_str!`; `/*PLATFORM*/` is replaced with
  `macos` or `windows` and the body gets `platform-<name>`. Rust pushes the whole `Model` as
  JSON through `window.onState`, toasts through `window.onToast`; the page sends requests with
  `send({action: …})`. CLAUDE.md: never declare a top-level `ipc` (wry owns `window.ipc`;
  WebKit would reject the whole script and show an empty window).
- WebViews: WKWebView (macOS system WebKit) and WebView2 (Chromium). Both support CSS custom
  properties, `color-mix()`, `backdrop-filter` (WebKit also needs `-webkit-backdrop-filter`),
  `:focus-visible`, `prefers-color-scheme`, `prefers-reduced-motion`, `prefers-contrast`.
  `prefers-reduced-transparency` is supported by WebView2 only; WebKit ignores it, which is
  harmless (progressive enhancement).
- The sibling UsbMemoryChecker uses the same stack and the same aurora style the current page
  copied (`src/ui/index.html` there: `.pill-btn`, `.modal-backdrop`, `.modal`, `.method`). Its
  update and donate pills sit at the right end of the title bar; `desktop-updates-donate`
  mirrors that placement, so this change leaves an action slot there.
- No sibling project loads anything from a CDN at runtime; neither will this page.
- Render functions build HTML strings with class names (`btn small primary`, `chip ok`,
  `card active`, `note`, `sub`, `dev dim`, `serial`, `err-line`, …) and `data-act` attributes
  that the single click handler dispatches on. Requests and `data-act` values are the contract
  with Rust and the click handler; class names are free to change but are kept where possible
  so the diff stays reviewable.

## Goals / Non-Goals

**Goals:**
- A concrete, implementable visual system: every color, size, radius, shadow and duration is
  a named token with a value given below, so the executor does not invent any.
- Same information and actions as now; the redesign is CSS plus markup, not behavior.
- AA contrast checked by a test, keyboard focus, reduced motion / contrast / transparency.
- An app icon for the `.app` (used by `release-pipeline`).

**Non-Goals:**
- No new data in the window except the round-trip time in the top-bar state (already in the
  model). No settings page, no theme switch, no new languages (ROADMAP keeps them).
- No change to the Android control screen beyond the launcher icon (decision 9).
- No web fonts, icon fonts, CSS frameworks or build step for the page.
- The update / Support pills and sheets themselves belong to `desktop-updates-donate`; this
  change defines the components they use (`.pill`, `.sheet`) and the empty slot.

## Decisions

### 1. Direction: "Ambient Glass"

A plausible 2035 desktop utility, extrapolated from where macOS (Liquid Glass), Windows 11
(Mica/Acrylic), Material 3 Expressive and spatial UIs are heading, minus the fashion of the
day:

- **Calm, content-first.** Chrome recedes: no rainbow gradients on controls, no drifting color
  blobs. One luminous accent ("ion", blue-violet) for the primary action and focus; a second
  brand hue ("mint") appears only in the mark and the live-link visuals (sparkline, link dot).
- **Light as material.** Surfaces are layered glass lit from above: translucent fill, backdrop
  blur, a 1 px specular top edge, soft long shadows. Depth comes from four elevation levels,
  not from borders everywhere.
- **Living status.** The connection is the hero: a "link island" in the top bar and a big
  tabular round-trip readout with an area sparkline. State marks breathe only while something
  is in progress.
- **Big, humane type.** Larger display sizes, sentence case instead of uppercase micro-labels,
  tabular numerals for every number, monospace only for addresses, ids and paths.
- **Soft geometry.** Continuous-curvature-looking radii (24/16/12/999), 8 px spacing grid,
  hit targets ≥ 28 px tall (WCAG 2.5.8 asks 24).

Alternatives considered: keep the aurora style (it is what UsbMemoryChecker looks like; Roman
asked for a redesign), neo-brutalism or "Y2K chrome" (trend-of-the-moment, poor fit for a
calm system utility), skeuomorphic hardware panel (heavy to draw in CSS, dated fast).

### 2. Color tokens

Defined as hex literals (WebKit on older macOS lacks `oklch()`; the values were picked in OKLCH
and converted). Dark is the base `:root` block; light overrides it. Translucent surfaces are
`rgba` over the canvas.

| Token | Dark | Light | Use |
|---|---|---|---|
| `--bg-0` | `#0B0C12` | `#F3F4F8` | window canvas, `body` background |
| `--bg-1` | `#12141C` | `#FFFFFF` | opaque base for surfaces (fallback, high contrast) |
| `--surface-1` | `rgba(255,255,255,0.06)` | `rgba(255,255,255,0.72)` | panels (glass) |
| `--surface-2` | `rgba(255,255,255,0.09)` | `#FFFFFF` | cards inside panels |
| `--surface-3` | `rgba(255,255,255,0.13)` | `#ECEEF4` | hovered controls, fields, code |
| `--surface-sheet` | `rgba(24,26,36,0.86)` | `rgba(255,255,255,0.90)` | sheets, toasts |
| `--edge` | `rgba(255,255,255,0.09)` | `rgba(16,18,32,0.08)` | hairline borders |
| `--edge-strong` | `rgba(255,255,255,0.18)` | `rgba(16,18,32,0.16)` | fields, hovered cards |
| `--spec` | `rgba(255,255,255,0.10)` | `rgba(255,255,255,0.90)` | 1 px top highlight (inset) |
| `--text-1` | `#F3F5FB` | `#11131B` | primary text |
| `--text-2` | `#CBD0E0` | `#41475C` | secondary text |
| `--text-3` | `#A8AEC4` | `#5A6179` | faint text (sub-lines, counts) |
| `--accent` | `#AFB4FF` | `#4A50DB` | primary buttons, focus, links, info |
| `--on-accent` | `#0B0C12` | `#FFFFFF` | text on `--accent` fill |
| `--mint` | `#5FE9C6` | `#067A62` | brand second hue, live link |
| `--ok` | `#4ADE8F` | `#09703F` | connected, permission granted |
| `--warn` | `#FFC266` | `#8F5400` | waiting, not paired, no permission |
| `--err` | `#FF9AA4` | `#C02B3C` | errors, danger buttons |
| `--support` | `#FF7DBA` | `#B92A68` | Support pill (used by `desktop-updates-donate`) |
| `--scrim` | `rgba(4,5,10,0.55)` | `rgba(20,22,40,0.28)` | sheet backdrop |

Measured contrast (WCAG 2.x), worst case over the real surface stacks and the three page
backdrops (canvas alone, with the mint glow, with the ion glow): panel = `--surface-1` over the
backdrop; card = `--surface-2` over the panel; sheet = `--surface-sheet` over the scrim over a card.
The first version of this table composited surfaces over `--bg-1` only, which overstated every
translucent dark surface; the test in decision 8 now computes the real stacks.

| Text | Dark panel / card / sheet | Light panel / card / sheet |
|---|---|---|
| `--text-1` | 13.3 / 10.1 / 15.8 | 17.7 / 18.5 / 17.5 |
| `--text-2` | 9.4 / 7.1 / 11.2 | 8.8 / 9.2 / 8.7 |
| `--text-3` | 6.6 / 5.0 / 7.8 | 5.9 / 6.1 / 5.8 |
| `--accent` | 6.3 / 4.8 / 8.9 | 5.2 / 5.4 / 5.7 |
| `--ok` | 7.1 / 5.4 / 10.0 | 5.3 / 5.5 / 5.8 |
| `--warn` | 7.6 / 5.8 / 10.8 | 5.2 / 5.5 / 5.8 |
| `--err` | 6.2 / 4.7 / 8.5 | 4.9 / 5.1 / 5.4 |
| `--support` | 6.1 / 4.6 / 7.3 | 5.6 / 5.8 / 5.5 |

Status colors on their own 8 % chip tint are included in the accent / ok / warn / err rows. The
ambient light (decision 4) is a 10 % color glow over the canvas and does lower contrast; it is
part of the measured backdrops above, not ignored. The test asks for 4.6:1 (4.5 plus a margin
for the `saturate(150%)` backdrop lift, which it does not model).

Chip and tint backgrounds: `color-mix(in srgb, var(--<status>) 8%, transparent)` with the
text in `var(--<status>)` and a border at 32 % (8 %, not 14 %: a stronger tint pulls the colored text
below 4.5:1 on dark cards). Danger button: transparent fill, text `--err`, hover fill
`color-mix(in srgb, var(--err) 8%, transparent)`.

**Test hook:** the same light values are also applied under `:root[data-theme="light"]`, and
the dark values under `:root[data-theme="dark"]` (overriding the media query), so a check can
force a theme with `document.documentElement.dataset.theme = 'light'` from
`HUBBRIDGE_DEV_SCRIPT`. Layout in CSS:

```css
:root { /* dark tokens */ color-scheme: dark; }
@media (prefers-color-scheme: light) { :root:not([data-theme="dark"]) { /* light tokens */ color-scheme: light; } }
:root[data-theme="light"] { /* light tokens, same values */ color-scheme: light; }
```

A rule inside a media query cannot share a selector list with one outside it, so the light
tokens are written twice; the token test (decision 8) asserts both copies are equal.

### 3. Typography, spacing, radii, elevation

- Fonts: `--font-ui: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif`;
  `--font-display: -apple-system, BlinkMacSystemFont, "SF Pro Display", "Segoe UI Variable Display", "Segoe UI", system-ui, sans-serif`;
  `--font-mono: ui-monospace, "SF Mono", "Cascadia Mono", Menlo, Consolas, monospace`.
- Scale (px / line-height / weight): `--fs-xs` 11.5/1.35/500 (chips), `--fs-s` 12.5/1.4/400
  (sub-lines, notes), `--fs-m` 14/1.45/400 (body), `--fs-l` 16/1.35/600 (card names, sheet
  titles), `--fs-xl` 20/1.25/650 (empty-state title), `--fs-2xl` 28/1.15/700 (connected phone
  name, letter-spacing −0.02em), `--fs-3xl` 44/1/700 (round-trip number, letter-spacing
  −0.03em, `font-variant-numeric: tabular-nums`). Section titles: `--fs-m`, 600, `--text-2`,
  sentence case (no `text-transform`), count badge after them.
- Numbers everywhere (`.num`, counters, RTT, counts): `font-variant-numeric: tabular-nums`.
- Spacing: 4 / 8 / 12 / 16 / 20 / 24 / 32 px (`--sp-1` … `--sp-7`). Panel padding 20, card
  padding 14, gaps 12–16.
- Radii: `--r-panel` 24, `--r-card` 16, `--r-control` 12, `--r-pill` 999, sheet 28.
- Control heights: button 32 (small 28), field 34, pill 30, icon tile 36.
- Elevation (box-shadow, always combined with `inset 0 1px 0 var(--spec)` on glass):
  - `--elev-1` (panels) dark `0 1px 2px rgba(0,0,0,.30), 0 16px 40px -18px rgba(0,0,0,.55)`,
    light `0 1px 2px rgba(20,24,40,.06), 0 16px 40px -20px rgba(20,24,40,.20)`;
  - `--elev-2` (cards on hover, active card) dark `0 8px 24px -12px rgba(0,0,0,.6)`, light
    `0 8px 24px -14px rgba(20,24,40,.22)`;
  - `--elev-3` (sheets) dark `0 40px 120px -30px rgba(0,0,0,.75)`, light
    `0 40px 100px -30px rgba(20,24,40,.30)`;
  - `--elev-4` (toasts) = `--elev-3`.
- Glass: panels `background: var(--surface-1); backdrop-filter: blur(28px) saturate(150%)`
  (plus `-webkit-` prefix); cards and fields are not blurred (cheaper, crisper text).

### 4. Canvas and ambient light

Replace the three animated blurred blobs with one fixed `::before` layer on `body`: two radial
gradients — mint at 20 % / 0 % (`rgba(95,233,198,0.08)` dark, `0.10` light, radius 60vmax) and
ion at 85 % / 100 % (`rgba(146,152,255,0.10)` dark, `0.10` light) — with a 60 s
`transform: translate3d()` drift of at most 3vmax, alternate, `ease-in-out`. No `filter: blur`
(gradients are already soft; blur on a full-window layer costs GPU on Windows). Under reduced
motion the layer is static.

### 5. Shell and screens

- **Top bar** (`header.top`, 52 px, drag region as today: mousedown on anything that is not a
  button / pill / input sends `drag`). Left: mark (28 px rounded tile, decision 7) + "HubBridge"
  (`--fs-l`, 650). Middle (`flex: 1`, centered): the **link island** — a pill (`id="status"`,
  height 30, `--surface-1` glass, `--edge`) with a state mark and text: idle → hollow ring
  `--text-3` + "Not connected"; connecting/reconnecting → `--warn` dot breathing (decision 6)
  + "Connecting to X…"; needs pairing → `--err` square-ish dot + "X needs pairing"; connected →
  `--ok` dot with a 4 px halo + "Name · Wi-Fi · 12.4 ms" (last RTT, `num`). Right:
  `<div class="top-actions" id="topActions"></div>`, empty in this change. The version label
  `#ver` moves into the action area as faint text until `desktop-updates-donate` replaces it
  with the version pill. macOS keeps the 86 px left padding for the window buttons.
- **Body grid:** `grid-template-columns: minmax(320px, 360px) 1fr; gap: 16px; padding: 8px 20px 20px`.
  Each column scrolls on its own (thin 8 px scrollbar, thumb `--edge-strong`).
- **Section header** (`.ph`): title (sentence case) + count badge (`--surface-3`, `--fs-xs`,
  min-width 20, radius pill) + spacer.
- **Agent / phone cards** (`.card`): `--surface-2`, `--r-card`, padding 14, gap 8; hover
  `--edge-strong` border; `.active` (connected agent) = 1.5 px `--accent` border + `--elev-2`
  + a 3 px accent bar on the left edge (shape, not color only). Name `--fs-l`; chips row;
  address `--font-mono` `--text-3`; actions row.
- **Connection panel (`#conn`):**
  - connected: two columns. Left: name `--fs-2xl`, model · Android · app version `--text-2`,
    chips (`Wi-Fi`/`ADB` ok chip, address mono chip, "no USB host" err chip), Disconnect
    (small). Right (min 280 px): RTT `--fs-3xl` with `ms` at `--fs-l`, caption "round trip",
    an **area sparkline** 280×64 (`polyline` stroke 2 px with a mint→ion linear gradient + a
    `polygon` fill of the same gradient at 18 % → 0 % vertical), stats line `min · avg · max`.
    Gradient stop colors come from the tokens (read with `getComputedStyle` once per render
    or written as `var()` in the SVG `stop-color` style).
  - idle: title "Connect a phone" `--fs-xl`, text, then the **path**: three step tiles in a row
    joined by a 2 px line behind them (the "bridge"); each tile has a 28 px numbered node
    (accent ring, number in `--accent`), bold step name, text.
  - connecting / reconnecting / needs pairing: centered title + error line + one button, as
    today, restyled.
- **USB devices:** grid `repeat(auto-fill, minmax(300px, 1fr))`, cards like above; icon tile
  36 px `--surface-3` with the stroke icon in `--accent`; Billboard cards are dimmed by
  `opacity .62` on the icon tile and the title color only (`--text-2`), not on the whole card: a
  whole-card opacity would pull the faint text below 4.5:1. They also carry the "hub info" chip
  (already text). Below 1000 px of window width the connection hero stacks to one column. Details tree in mono `--fs-s`, indented 12/24 px with a
  1 px `--edge` guide line. Serial row: separated by a hairline, form controls in one wrap row.
- **Notes** (`.note`): `--surface-2`, dashed `--edge-strong` 1 px border, `--fs-s`.
- **Toasts:** bottom-right stack, `--surface-sheet` + blur, radius 16, `--elev-4`, a 6 px
  status dot (ok/err) before the text plus the existing text; error toasts stay 12 s, others 6 s
  (unchanged); click dismisses.
- **Sheet** (opens with focus on the first field, Tab is trapped inside, closing returns focus to the opener; `.modal` backdrop `--scrim` + blur 8 px; `.sheet` 420 px wide, radius 28,
  padding 24, `--surface-sheet` + blur 32 px, `--elev-3`): head row (title `--fs-l` + optional
  icon tile + close button 30 px "✕" with `aria-label`), body, actions row right-aligned. The
  PIN field: mono 32 px, letter-spacing 14 px, centered. `desktop-updates-donate` reuses
  `.modal`/`.sheet` for its two sheets.

### 6. Components and states

- **Buttons** (`.btn`): height 32, padding 0 14, radius 12, `--fs-m` 550, border 1 px
  `--edge-strong`, fill `--surface-3`. Hover: fill `color-mix(in srgb, var(--surface-3) 80%, var(--accent) 10%)`.
  Active: `transform: scale(.97)` (no transform under reduced motion). Disabled: opacity .45,
  no pointer. `.primary`: fill `--accent`, text `--on-accent`, no border, hover
  `filter: brightness(1.08)`. `.ghost`: transparent. `.danger`: text `--err`. `.small`:
  height 28, padding 0 10, radius 10, `--fs-s`. Busy spinner: 12 px ring, `currentColor`.
- **Chips** (`.chip`): height 22, padding 0 8, radius pill, `--fs-xs`, neutral = `--edge-strong`
  border + `--text-2`; `.ok/.warn/.err/.info` as decision 2.
- **Fields** (`input.field`): height 34, radius 12, `--surface-3` fill, `--edge-strong` border;
  focus: border `--accent` + `0 0 0 3px color-mix(in srgb, var(--accent) 24%, transparent)`.
- **Pill** (`.pill`, for the top bar and the action area): height 30, padding 0 12, radius
  pill, glass `--surface-1`, `--edge`, `--fs-s` `--text-2`; hover `--surface-2` + `--text-1`.
- **Disclosure** (`details > summary`): chevron drawn with CSS (rotates 90° on open, 200 ms).
- **Focus:** `:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }` on
  buttons, summaries, checkboxes; fields use their focus ring above; `:focus:not(:focus-visible)`
  shows nothing. Accent vs panel contrast 6.2 (dark) / 5.9 (light) ≥ 3:1.
- **State marks:** 8 px dots; connected has a halo `0 0 0 4px color-mix(… --ok 18%)`;
  connecting "breathes" (opacity 1 → .35, 1.6 s ease-in-out infinite).
- **Motion tokens:** `--dur-1: 120ms` (hover, press), `--dur-2: 200ms` (fades, toasts,
  chevron), `--dur-3: 320ms` (sheet in); `--ease: cubic-bezier(.2,.8,.2,1)`;
  `--ease-emph: cubic-bezier(.3,1.25,.5,1)` (sheet scale .96 → 1 and translateY 8 → 0). Toast
  enters with translateY 8 → 0 + fade. No animation on data updates (numbers just change).

### 6b. Preference overrides

- `@media (prefers-reduced-motion: reduce)`: ambient layer static; `animation: none` on the
  state dot, spinners (`.spin` becomes a static ring with a 25 % gap) and toasts;
  `transition-duration` ≤ 120 ms and only `opacity`/`color`/`background-color`; sheet and toast
  appear with a fade, no scale or translate; `.btn:active` no transform.
- `@media (prefers-contrast: more)` and `@media (prefers-reduced-transparency: reduce)`:
  `backdrop-filter: none` everywhere; opaque surfaces — dark `--surface-1 #1A1C25`,
  `--surface-2 #22252F`, `--surface-3 #2B2E3A`, `--surface-sheet #1A1C25`; light
  `--surface-1 #FFFFFF`, `--surface-2 #FFFFFF`, `--surface-3 #E6E8EF`, `--surface-sheet #FFFFFF`;
  `--edge` takes the `--edge-strong` value; ambient layer hidden. Contrast-more only:
  `--text-3` takes the `--text-2` value and borders become 1.5 px.

### 7. Mark and app icon

- Mark: two filled nodes (circles r = 2.2 on a 24-unit grid at x = 5 and x = 19, y = 16) joined
  by an arc (`M5 16 C 8 7, 16 7, 19 16`, stroke 2.2, round caps) above a short baseline
  (`M3 20h18`, stroke 1.6, 50 % opacity) — a bridge between two devices. In the page: inline
  SVG, stroke/fill `currentColor` = `--on-accent` (dark ink on the dark theme's light gradient, white on
  the light theme's saturated one; dark ink on the light gradient measured ~3:1) on a 28 px rounded tile (radius 9)
  filled with `linear-gradient(135deg, var(--mint), var(--accent))`.
- App icon: `scripts/make-icon.swift` draws 1024×1024: macOS-style rounded square (radius
  ~22.4 %, 100 px inset margin as Apple's grid), background a vertical gradient `#1A1D2B` →
  `#0B0C12`, a soft ion glow behind the mark, the mark in a mint→ion gradient. Output
  `crates/desktop/assets/icon.png`; `scripts/make-icon.sh` makes `AppIcon.icns` with `sips` +
  `iconutil` exactly like UsbMemoryChecker's script (sizes 16…512 @1x/@2x). Both files are
  committed (CI does not run the script). No `.ico`: there is no Windows release
  (`release-pipeline`); adding one later is one more step in the script.
- Android: an adaptive icon (`mipmap-anydpi-v26/ic_launcher.xml` and `ic_launcher_round.xml`, minSdk 26)
  with a `#12141C` background layer, a foreground layer with the same mark (mint→ion gradients via
  `<aapt:attr>`, kept inside the 66 dp safe zone) and a `<monochrome>` layer for Android 13 themed icons.

### 8. Guard tests in Rust

In `crates/desktop/src/main.rs` (`#[cfg(test)] mod tests`, the page is `include_str!`):
- `page_declares_no_top_level_ipc`: no line of the `<script>` declares `ipc` with
  `const|let|var|function|class`, and none assigns `window.ipc =` / `globalThis.ipc =` / `self.ipc =`.
- `page_loads_nothing_remote`: the page has no `src=`/`href=` or `url(` starting with `http` or `//`,
  no `@import` and no `<link rel="stylesheet"`.
- `theme_tokens_meet_contrast`: reads the full declaration maps of the dark `:root`, the forced
  dark copy (must be a subset of the base with equal values), both light copies (must be equal) and
  both opaque overrides (dark; the two light copies must be equal). It composites the real surface
  stacks (canvas, each ambient glow, `--surface-1`, `--surface-2`, `--surface-3`, sheet over scrim,
  toast) and requires 4.6:1 for every text and status color on every surface it is used on, for the
  8 % status chip tints, the danger-button hover, `--on-accent` on `--accent` (also brightened by 8 %),
  and in the opaque (increased contrast / reduced transparency) variants. A tiny
  `fn contrast(a, b) -> f64` (WCAG relative luminance) and an alpha `over()` live in the test module.

These are plain string tests, cheap, and catch the two failure modes that leave an empty or
unreadable window.

### 9. Android scope

The Android app's UI is a 172-line programmatic `LinearLayout` of platform `Button`s and
`TextView`s on `Theme.DeviceDefault.DayNight`; it has no styles, colors or layouts to retheme.
A redesign there means writing a new view layer (custom drawables, theme, maybe Compose, which
the project avoids: "no AndroidX"), touching `MainActivity` logic, and testing on a phone —
a separate change with its own risk. For the first release the phone screen is a short-lived
setup step (start, PIN); the desktop window is where the user spends time. So: launcher icon
only (decision 7). A follow-up item is added to ROADMAP ("Android control screen in the
desktop's visual language").

## Risks / Trade-offs

- [Backdrop blur on many panels costs GPU on low-end Windows] → blur only on panels, top-bar
  pills, sheets and toasts (≤ 6 layers), never on cards; reduced transparency removes it.
- [The light tokens are written twice] → the token test asserts the copies are identical.
- [WebKit ignores `prefers-reduced-transparency`] → macOS users get the same result with
  "Increase contrast" (`prefers-contrast: more`), which WebKit supports; noted in docs/ARCHITECTURE.md.
- [Re-rendering the top-bar island every RTT update could flicker the dot animation] → the
  island uses `put()` (string-compare) like today; the animated dot keeps its element as long as
  the state class is unchanged (render the dot and the text in two separate `put()` targets).
- [Restyle silently drops an action] → spec scenario "Every action still there" is checked
  against the simulator with a list of all `data-act` values and ids before and after
  (`rg -o 'data-act|act: "[a-z_]+"'`).

## Migration Plan

Pure UI change: no settings or data migration. Rollback = revert the commit.
