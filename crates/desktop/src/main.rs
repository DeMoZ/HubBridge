//! HubBridge desktop window.
//!
//! tao + wry (system WebView: WKWebView on macOS, WebView2 on Windows), page in
//! `ui/index.html`. The WebView lives on the main thread only: the controller ([`app::App`],
//! tokio) sends scripts through the `EventLoopProxy`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use std::sync::Arc;

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

#[derive(Debug)]
enum UserEvent {
    Js(String),
    DragWindow,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("HUBBRIDGE_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let push: app::Push = Arc::new(move |script| {
        let _ = proxy.send_event(UserEvent::Js(script));
    });
    let app = {
        let _guard = rt.enter();
        app::App::new(rt.handle().clone(), push)?
    };

    #[allow(unused_mut)]
    let mut builder = WindowBuilder::new()
        .with_title("HubBridge")
        .with_inner_size(tao::dpi::LogicalSize::new(1180.0, 780.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(900.0, 600.0));
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowBuilderExtMacOS;
        builder = builder
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true);
    }
    let window = builder.build(&event_loop)?;

    let ipc_app = Arc::clone(&app);
    let drag_proxy = event_loop.create_proxy();
    let platform = if cfg!(target_os = "macos") { "macos" } else { "windows" };
    let page = include_str!("ui/index.html").replace("/*PLATFORM*/", platform);
    let webview = WebViewBuilder::new()
        .with_html(page)
        .with_ipc_handler(move |msg| match serde_json::from_str::<app::Request>(msg.body()) {
            Ok(app::Request::Drag) => {
                let _ = drag_proxy.send_event(UserEvent::DragWindow);
            }
            Ok(req) => ipc_app.handle(req),
            Err(e) => eprintln!("bad message from the window: {e}: {}", msg.body()),
        })
        .build(&window)?;

    // For screenshots and checks: a script to run in the page after start.
    if let Ok(script) = std::env::var("HUBBRIDGE_DEV_SCRIPT") {
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(3000));
            let _ = proxy.send_event(UserEvent::Js(script));
        });
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _keep = (&rt, &app);
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::UserEvent(UserEvent::Js(script)) => {
                let _ = webview.evaluate_script(&script);
            }
            Event::UserEvent(UserEvent::DragWindow) => {
                let _ = window.drag_window();
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    const PAGE: &str = include_str!("ui/index.html");

    /// The `<script>` part of the page.
    fn script() -> &'static str {
        let start = PAGE.find("<script>").expect("page has a script") + "<script>".len();
        let end = PAGE.rfind("</script>").expect("script is closed");
        &PAGE[start..end]
    }

    /// True when a script line declares, binds or assigns the global name `ipc`.
    fn ipc_violation(line: &str) -> bool {
        let l = line.trim_start();
        let ident_at = |i: usize| {
            l[i..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect::<String>()
        };
        for kw in ["const", "let", "var", "function", "class"] {
            if let Some(rest) = l.strip_prefix(kw)
                && rest.starts_with(char::is_whitespace)
            {
                let name = rest.trim_start();
                if name
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                    .collect::<String>()
                    == "ipc"
                {
                    return true;
                }
                if kw == "const" || kw == "let" || kw == "var" {
                    // `let a = 1, ipc = 2`, `const { ipc } = x`, `const [ipc] = x`: `ipc` as a binding name.
                    let mut from = 0;
                    while let Some(at) = l[from..].find("ipc") {
                        let i = from + at;
                        let before = l[..i].trim_end().chars().last();
                        let word_start = !l[..i]
                            .chars()
                            .last()
                            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
                        if word_start && ident_at(i) == "ipc" && matches!(before, Some(',' | '{' | '[')) {
                            return true;
                        }
                        from = i + 3;
                    }
                }
            }
        }
        for target in [
            "window.ipc",
            "globalThis.ipc",
            "self.ipc",
            "window[\"ipc\"]",
            "window['ipc']",
            "globalThis[\"ipc\"]",
            "globalThis['ipc']",
            "self[\"ipc\"]",
            "self['ipc']",
        ] {
            let mut from = 0;
            while let Some(at) = l[from..].find(target) {
                let rest = l[from + at + target.len()..].trim_start();
                if rest.starts_with('=') && !rest.starts_with("==") {
                    return true;
                }
                from += at + target.len();
            }
        }
        l.contains("defineProperty(window") && (l.contains("\"ipc\"") || l.contains("'ipc'"))
    }

    #[test]
    fn page_declares_no_top_level_ipc() {
        // wry owns the non-configurable global `window.ipc`; a top-level declaration or assignment of
        // that name makes WebKit reject the whole script (empty window, no error).
        for line in script().lines() {
            assert!(
                !ipc_violation(line),
                "`ipc` declared or assigned in the page script: {line}"
            );
        }
    }

    #[test]
    fn ipc_scan_catches_its_probes() {
        for bad in [
            "const ipc = 1;",
            "let a = 1, ipc = 2;",
            "const { ipc } = window;",
            "const { a, ipc: b } = x;",
            "var ipc;",
            "function ipc() {}",
            "class ipc {}",
            "window.ipc = 3;",
            "globalThis.ipc=3;",
            "window[\"ipc\"] = 3;",
            "self['ipc'] = 3;",
            "Object.defineProperty(window, \"ipc\", {});",
        ] {
            assert!(ipc_violation(bad), "must be caught: {bad}");
        }
        for ok in [
            "const send = (o) => window.ipc.postMessage(JSON.stringify(o));",
            "if (window.ipc == null) return;",
            "const ipcs = 1;",
        ] {
            assert!(!ipc_violation(ok), "must not be flagged: {ok}");
        }
    }

    /// The first thing in `text` that loads from the network, if any.
    fn remote_violation(text: &str) -> Option<String> {
        let t = text.to_ascii_lowercase();
        for needle in [
            "@import",
            "srcset",
            "new websocket(",
            "xmlhttprequest",
            "new eventsource(",
            "sendbeacon(",
            "<link rel=\"stylesheet\"",
            "<link rel='stylesheet'",
        ] {
            if t.contains(needle) {
                return Some(needle.to_string());
            }
        }
        let is_remote = |rest: &str| {
            let r = rest.trim_start().trim_start_matches(['"', '\'', '`']).trim_start();
            r.starts_with("http") || r.starts_with("//") || r.starts_with("ws:") || r.starts_with("wss:")
        };
        for (pat, label) in [
            ("src=", "src="),
            ("href=", "href="),
            ("url(", "url("),
            (".src=", ".src ="),
            ("fetch(", "fetch("),
        ] {
            let mut from = 0;
            while let Some(at) = t[from..].find(pat) {
                let rest = &t[from + at + pat.len()..];
                if is_remote(rest) {
                    return Some(format!("{label} remote"));
                }
                from += at + pat.len();
            }
        }
        // `el.src = "http..."` with spaces around `=`.
        let squeezed: String = t.split_whitespace().collect::<Vec<_>>().join(" ").replace(" = ", "=");
        if squeezed != t {
            let mut from = 0;
            while let Some(at) = squeezed[from..].find(".src=") {
                if is_remote(&squeezed[from + at + 5..]) {
                    return Some(".src = remote".into());
                }
                from += at + 5;
            }
        }
        None
    }

    #[test]
    fn page_loads_nothing_remote() {
        if let Some(found) = remote_violation(PAGE) {
            panic!("the page must not load anything remote: {found}");
        }
    }

    #[test]
    fn remote_scan_catches_its_probes() {
        for bad in [
            "<img src=\"https://x/y.png\">",
            "<img src=http://x/y.png>",
            "<link href=\"//cdn/x.css\">",
            "<img srcset=\"a.png 2x\">",
            "a { background: url( http://x/y.png ); }",
            "a { background: url(\"//x/y.png\"); }",
            "el.src = \"https://x/a.js\";",
            "el.src='http://x/a.js'",
            "fetch(\"https://x\")",
            "fetch('http://x')",
            "new WebSocket('wss://x')",
            "const r = new XMLHttpRequest();",
            "@import url(x.css);",
        ] {
            assert!(remote_violation(bad).is_some(), "must be caught: {bad}");
        }
        for ok in [
            "a { background: url(#spark-g); }",
            "const g = \"url(#spark-g)\";",
            "<svg viewBox=\"0 0 1 1\"></svg>",
        ] {
            assert!(remote_violation(ok).is_none(), "must not be flagged: {ok}");
        }
    }

    #[test]
    fn only_the_live_region_is_a_status_in_the_top_bar() {
        let start = PAGE.find("<header class=\"top\"").expect("top bar");
        let end = start + PAGE[start..].find("</header>").expect("top bar closes");
        let header = &PAGE[start..end];
        assert_eq!(
            header.matches("role=\"status\"").count(),
            1,
            "exactly one role=\"status\" in the top bar"
        );
        assert!(!header.contains("aria-live"), "no aria-live in the top bar");
        let at = header.find("role=\"status\"").unwrap();
        let tag_start = header[..at].rfind('<').unwrap();
        let tag = &header[tag_start..at];
        assert!(
            tag.contains("id=\"stLive\""),
            "the live region must be #stLive, found `{tag}`"
        );
    }

    // ---- colors -------------------------------------------------------------------------------

    type Rgb = [f64; 3];

    fn channel(v: f64) -> f64 {
        let c = v / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(c: Rgb) -> f64 {
        0.2126 * channel(c[0]) + 0.7152 * channel(c[1]) + 0.0722 * channel(c[2])
    }

    /// WCAG 2.x contrast ratio of two opaque colors.
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn contrast_helper_is_correct() {
        assert!((contrast([0.0; 3], [255.0; 3]) - 21.0).abs() < 0.01);
        assert!((contrast([119.0; 3], [255.0; 3]) - 4.48).abs() < 0.01);
    }

    /// Alpha-composite `fg` (color, alpha) over an opaque `bg`.
    fn over(fg: (Rgb, f64), bg: Rgb) -> Rgb {
        let (c, a) = fg;
        [0, 1, 2].map(|i| c[i] * a + bg[i] * (1.0 - a))
    }

    /// `#rrggbb` or `rgba(r,g,b,a)` (whitespace removed).
    fn parse_color(v: &str) -> Option<(Rgb, f64)> {
        if let Some(h) = v.strip_prefix('#') {
            if h.len() != 6 {
                return None;
            }
            let n = u32::from_str_radix(h, 16).ok()?;
            return Some((
                [f64::from(n >> 16 & 255), f64::from(n >> 8 & 255), f64::from(n & 255)],
                1.0,
            ));
        }
        let inner = v.strip_prefix("rgba(")?.strip_suffix(')')?;
        let p: Vec<f64> = inner.split(',').map(|x| x.parse().ok()).collect::<Option<_>>()?;
        (p.len() == 4).then(|| ([p[0], p[1], p[2]], p[3]))
    }

    // ---- reading the CSS ----------------------------------------------------------------------

    /// The text between the braces of the first rule whose selector starts at `marker`, searching
    /// from byte offset `from`.
    fn block_at(from: usize, marker: &str) -> &'static str {
        let at = from
            + PAGE[from..]
                .find(marker)
                .unwrap_or_else(|| panic!("page has no `{marker}` block"));
        let open = at + PAGE[at..].find('{').expect("block opens");
        let mut depth = 0;
        for (i, ch) in PAGE[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &PAGE[open + 1..open + i];
                    }
                }
                _ => {}
            }
        }
        panic!("block `{marker}` is not closed");
    }

    /// Every `--name: value;` declaration of a block, value without whitespace.
    fn decls(block: &str) -> BTreeMap<String, String> {
        block
            .split(';')
            .filter_map(|d| {
                let (name, value) = d.split_once(':')?;
                let name = name.trim();
                name.starts_with("--")
                    .then(|| (name.to_string(), value.split_whitespace().collect::<String>()))
            })
            .collect()
    }

    const OPAQUE_MEDIA: &str = "@media (prefers-contrast: more), (prefers-reduced-transparency: reduce) {";

    struct Themes {
        dark: BTreeMap<String, String>,
        light: BTreeMap<String, String>,
        dark_opaque: BTreeMap<String, String>,
        light_opaque: BTreeMap<String, String>,
    }

    fn themes() -> Themes {
        let dark = decls(block_at(0, ":root {"));
        let forced_dark = decls(block_at(0, ":root[data-theme=\"dark\"] {"));
        for (k, v) in &forced_dark {
            assert_eq!(dark.get(k), Some(v), "forced dark `{k}` differs from the base :root");
        }
        let light = decls(block_at(0, ":root:not([data-theme=\"dark\"])"));
        let forced_light = decls(block_at(0, ":root[data-theme=\"light\"]"));
        assert_eq!(
            light, forced_light,
            "the two copies of the light tokens must be identical"
        );

        let om = PAGE.find(OPAQUE_MEDIA).expect("opaque override media query");
        let dark_opaque = decls(block_at(om, ":root, :root[data-theme=\"dark\"]"));
        let lo_forced = decls(block_at(om, ":root[data-theme=\"light\"]"));
        let lo_media = decls(block_at(om, ":root:not([data-theme=\"dark\"])"));
        assert_eq!(
            lo_forced, lo_media,
            "the two copies of the opaque light overrides must be identical"
        );
        Themes {
            dark,
            light,
            dark_opaque,
            light_opaque: lo_forced,
        }
    }

    // ---- contrast of real surfaces ------------------------------------------------------------

    /// Minimum contrast asked for: 4.5 plus a margin for the `saturate(150%)` backdrop lift and
    /// rendering differences that this model does not compute.
    const MIN: f64 = 4.6;

    /// How the CSS rules use colors, read from the page so the test follows the CSS.
    struct Usage {
        /// `.chip.<status>` background tint (fraction) per status token name.
        chip_tint: BTreeMap<&'static str, f64>,
        /// `.btn.danger:hover` background tint (fraction of `--err`).
        danger_hover: f64,
        /// `.btn:hover` background: `color-mix(in srgb, var(--surface-3) A%, var(--accent) B%)`.
        hover_mix: (f64, f64),
    }

    /// The number (percent) written right after `needle` in `block`.
    fn percent_after(block: &str, needle: &str) -> f64 {
        let at = block
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not found in `{block}`"));
        let num: String = block[at + needle.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        num.parse::<f64>()
            .unwrap_or_else(|_| panic!("no percentage after `{needle}`"))
    }

    fn usage() -> Usage {
        let mut chip_tint = BTreeMap::new();
        for (status, token) in [
            ("ok", "--ok"),
            ("warn", "--warn"),
            ("err", "--err"),
            ("info", "--accent"),
        ] {
            let block = block_at(0, &format!(".chip.{status} {{"));
            let pct = percent_after(block, &format!("background: color-mix(in srgb, var({token}) "));
            chip_tint.insert(token, pct / 100.0);
        }
        let danger_hover = percent_after(
            block_at(0, ".btn.danger:hover {"),
            "background: color-mix(in srgb, var(--err) ",
        ) / 100.0;
        let hover = block_at(0, ".btn:hover {");
        let hover_mix = (
            percent_after(hover, "color-mix(in srgb, var(--surface-3) "),
            percent_after(hover, "var(--accent) "),
        );
        Usage {
            chip_tint,
            danger_hover,
            hover_mix,
        }
    }

    /// `color-mix(in srgb, c1 p1%, c2 p2%)` with premultiplied alpha as in CSS Color 5 (percentages that
    /// add up to less than 100 scale the result's alpha).
    fn color_mix(c1: (Rgb, f64), p1: f64, c2: (Rgb, f64), p2: f64) -> (Rgb, f64) {
        let total = p1 + p2;
        let (w1, w2) = (p1 / total, p2 / total);
        let alpha = w1 * c1.1 + w2 * c2.1;
        let rgb = [0, 1, 2].map(|i| (w1 * c1.0[i] * c1.1 + w2 * c2.0[i] * c2.1) / alpha);
        (rgb, alpha * (total / 100.0).min(1.0))
    }

    fn tok(t: &BTreeMap<String, String>, name: &str) -> (Rgb, f64) {
        parse_color(t.get(name).unwrap_or_else(|| panic!("token {name} missing")))
            .unwrap_or_else(|| panic!("token {name} is not a color"))
    }

    fn solid(t: &BTreeMap<String, String>, name: &str) -> Rgb {
        let (c, a) = tok(t, name);
        assert_eq!(a, 1.0, "{name} must be opaque");
        c
    }

    /// Checks every text color on every surface it is drawn on (`t` already has any opaque overrides
    /// merged in). Returns the failures.
    fn check_theme(label: &str, t: &BTreeMap<String, String>, u: &Usage) -> Vec<String> {
        let mut fails = Vec::new();
        let mut want = |what: String, fg: Rgb, bg: Rgb| {
            let r = contrast(fg, bg);
            if r < MIN {
                fails.push(format!("{label}: {what} is {r:.2}:1, needs {MIN}:1"));
            }
        };
        let bg0 = solid(t, "--bg-0");
        // Page behind the panels: the canvas alone and with each ambient glow over it.
        let mut bases = vec![("canvas", bg0)];
        for g in ["--glow-a", "--glow-b"] {
            bases.push((g, over(tok(t, g), bg0)));
        }
        let texts = [
            "--text-1",
            "--text-2",
            "--text-3",
            "--accent",
            "--ok",
            "--warn",
            "--err",
            "--support",
        ];
        for (bname, base) in bases {
            let panel = over(tok(t, "--surface-1"), base);
            let card = over(tok(t, "--surface-2"), panel);
            let ctl_panel = over(tok(t, "--surface-3"), panel);
            let ctl_card = over(tok(t, "--surface-3"), card);
            // Sheets and toasts: the sheet fill over the scrim over a card (worst of card and panel).
            let scrimmed = over(tok(t, "--scrim"), card);
            let sheet = over(tok(t, "--surface-sheet"), scrimmed);
            let toast = over(tok(t, "--surface-sheet"), card);
            for (sname, surf) in [("panel", panel), ("card", card), ("sheet", sheet), ("toast", toast)] {
                for name in texts {
                    want(format!("{name} on {sname} ({bname})"), solid(t, name), surf);
                }
            }
            for (sname, surf) in [("control on panel", ctl_panel), ("control on card", ctl_card)] {
                for name in ["--text-1", "--text-2"] {
                    want(format!("{name} on {sname} ({bname})"), solid(t, name), surf);
                }
            }
            // Text drawn straight on the canvas (the version label, the brand name) and placeholders in fields.
            for name in ["--text-1", "--text-3"] {
                want(format!("{name} on the canvas ({bname})"), solid(t, name), base);
            }
            for (sname, surf) in [("field on panel", ctl_panel), ("field on card", ctl_card)] {
                want(
                    format!("placeholder (--text-2) on {sname} ({bname})"),
                    solid(t, "--text-2"),
                    surf,
                );
            }
            // Status chips: the tint the CSS gives each `.chip.<status>` over a card or a panel.
            for name in ["--ok", "--warn", "--err", "--accent"] {
                let (c, _) = tok(t, name);
                let tint = u.chip_tint[name];
                for (sname, surf) in [("card", card), ("panel", panel)] {
                    want(
                        format!("chip text {name} on {:.0}% tint over {sname} ({bname})", tint * 100.0),
                        c,
                        over((c, tint), surf),
                    );
                }
                // Danger button hover: the tint of `.btn.danger:hover`.
                if name == "--err" {
                    for (sname, surf) in [("card", card), ("panel", panel)] {
                        want(
                            format!(
                                "danger button hover ({:.0}% tint) on {sname} ({bname})",
                                u.danger_hover * 100.0
                            ),
                            c,
                            over((c, u.danger_hover), surf),
                        );
                    }
                }
            }
            // Default button hover: color-mix(surface-3 A%, accent B%) under the button text.
            let hover = color_mix(tok(t, "--surface-3"), u.hover_mix.0, tok(t, "--accent"), u.hover_mix.1);
            for (sname, surf) in [("panel", panel), ("card", card)] {
                want(
                    format!("--text-1 on hovered button over {sname} ({bname})"),
                    solid(t, "--text-1"),
                    over(hover, surf),
                );
            }
        }
        // Filled primary button, also when brightened by 8 % on hover.
        let accent = solid(t, "--accent");
        let on = solid(t, "--on-accent");
        want("--on-accent on --accent".to_string(), on, accent);
        want(
            "--on-accent on hovered --accent".to_string(),
            on,
            accent.map(|v| (v * 1.08).min(255.0)),
        );
        fails
    }

    #[test]
    fn theme_tokens_meet_contrast() {
        let th = themes();
        let u = usage();
        let merge = |base: &BTreeMap<String, String>, over: &BTreeMap<String, String>| {
            let mut m = base.clone();
            m.extend(over.iter().map(|(k, v)| (k.clone(), v.clone())));
            m
        };
        let mut fails = Vec::new();
        fails.extend(check_theme("dark", &th.dark, &u));
        fails.extend(check_theme("light", &merge(&th.dark, &th.light), &u));
        // Increased contrast / reduced transparency: opaque surfaces (text-3 keeps its own value here;
        // with more contrast it becomes text-2, which is checked anyway).
        fails.extend(check_theme("dark opaque", &merge(&th.dark, &th.dark_opaque), &u));
        fails.extend(check_theme(
            "light opaque",
            &merge(&merge(&th.dark, &th.light), &th.light_opaque),
            &u,
        ));
        assert!(fails.is_empty(), "contrast failures:\n{}", fails.join("\n"));
    }

    /// Text colors in the stylesheet must be plain tokens: no `color-mix`, alpha or literal colors, so the
    /// token-level contrast test also covers how the colors are used.
    #[test]
    fn text_colors_are_plain_tokens() {
        let start = PAGE.find("<style>").expect("style") + 7;
        let css = &PAGE[start..PAGE.find("</style>").expect("style closes")];
        let allowed = [
            "var(--text-1)",
            "var(--text-2)",
            "var(--text-3)",
            "var(--accent)",
            "var(--ok)",
            "var(--warn)",
            "var(--err)",
            "var(--support)",
            "var(--on-accent)",
            "inherit",
            "currentColor",
        ];
        let mut from = 0;
        let mut bad = Vec::new();
        while let Some(at) = css[from..].find("color:") {
            let i = from + at;
            from = i + 6;
            // Only the `color` property: not `background-color:`, `stop-color:`, `color-scheme:`, `border-color:`.
            if css[..i].chars().last().is_some_and(|c| c.is_alphanumeric() || c == '-') {
                continue;
            }
            let value: String = css[from..].chars().take_while(|c| *c != ';' && *c != '}').collect();
            let value = value.trim();
            if !allowed.contains(&value) {
                bad.push(value.to_string());
            }
        }
        assert!(bad.is_empty(), "text colors must be plain tokens, found: {bad:?}");
    }

    #[test]
    fn color_mix_matches_css_for_a_tint() {
        // `color-mix(in srgb, X 8%, transparent)` is X at alpha 0.08; `A 80%, B 10%` of opaque colors is
        // their 8:1 mix at alpha 0.9.
        let x = ([10.0, 20.0, 30.0], 1.0);
        let (rgb, a) = color_mix(x, 8.0, ([0.0; 3], 0.0), 92.0);
        assert!((a - 0.08).abs() < 1e-9 && (rgb[0] - 10.0).abs() < 1e-9);
        let (rgb, a) = color_mix(([90.0; 3], 1.0), 80.0, ([0.0; 3], 1.0), 10.0);
        assert!((a - 0.9).abs() < 1e-9 && (rgb[0] - 80.0).abs() < 1e-9);
    }

    /// The body of the top-level JS function `name` in the page script.
    fn js_function(name: &str) -> &'static str {
        let sc = script();
        let at = sc
            .find(&format!("function {name}("))
            .unwrap_or_else(|| panic!("function {name} missing"));
        let end = at + sc[at..].find("\n}\n").expect("function ends");
        &sc[at..end]
    }

    #[test]
    fn open_sheet_makes_the_page_inert_and_close_sheet_undoes_it() {
        assert!(
            js_function("openSheet").contains("setInert(true)"),
            "openSheet must make the page inert"
        );
        assert!(
            js_function("closeSheet").contains("setInert(false)"),
            "closeSheet must clear inert"
        );
        assert!(
            script().contains("el.inert = on"),
            "setInert must set the inert property"
        );
    }

    #[test]
    fn every_put_host_has_a_focusable_fallback() {
        let sc = script();
        let mut hosts = std::collections::BTreeSet::new();
        for part in sc.split("put(\"").skip(1) {
            if let Some(end) = part.find('"') {
                hosts.insert(&part[..end]);
            }
        }
        assert!(!hosts.is_empty(), "no put() hosts found");
        for id in hosts {
            let at = PAGE
                .find(&format!("id=\"{id}\""))
                .unwrap_or_else(|| panic!("put host #{id} missing in the markup"));
            let start = PAGE[..at].rfind("<section").unwrap_or(0);
            let section = &PAGE[start..at + PAGE[at..].find("</section>").unwrap_or(0)];
            if id.starts_with("st") {
                continue;
            }
            assert!(
                section.contains("tabindex=\"-1\""),
                "panel of #{id} has no focusable heading or tabindex=-1 fallback"
            );
        }
    }
}
