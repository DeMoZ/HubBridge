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
