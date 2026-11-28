//! Phone displays and screen mirroring in the window: what the "Phone displays" panel shows
//! and the controller behind its Mirror and Stop buttons.
//!
//! One mirror at a time. Capture and encoder callbacks run on their own queues and never touch
//! the WebView; the page learns everything from the model.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hubbridge_host::mirror::session::require_new_app;
use hubbridge_host::mirror::stats::Snapshot;
use hubbridge_host::mirror::{self, MirrorHandle, MirrorOptions, MirrorSession, ScreenPermissionDenied};
use hubbridge_host::{Client, ClientEvent};
use hubbridge_proto::{AgentInfo, DisplayInfo, DisplayList};
use serde::Serialize;

use super::{App, STARTING_WAIT, lock, short};

/// The only address the window opens in System Settings.
const SCREEN_SETTINGS_URL: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
/// How long stopping the mirror may hold up a disconnect, a quit or an update.
const STOP_LIMIT: Duration = Duration::from_secs(3);
/// The busy key of a mirror that is starting.
const BUSY: &str = "mirror";

/// The phone's displays, while connected.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DisplaysView {
    /// The connected phone app knows displays.
    pub supported: bool,
    pub list: Vec<DisplayInfo>,
    /// Samsung DeX is on.
    pub desktop_mode: bool,
}

impl DisplaysView {
    pub(super) fn from_list(list: DisplayList) -> DisplaysView {
        DisplaysView {
            supported: true,
            list: list.displays,
            desktop_mode: list.desktop_mode,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MirrorState {
    #[default]
    Idle,
    Starting,
    Running,
    /// It ran and ended by itself: `reason` says why.
    Stopped,
    /// It did not start: `reason` says why.
    Failed,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MirrorView {
    pub state: MirrorState,
    /// Display id of the running or starting mirror.
    pub display: Option<u32>,
    pub display_name: Option<String>,
    /// Stream size, e.g. `1728×1080`.
    pub size: Option<String>,
    pub fps: u32,
    /// Once a second while running.
    pub stats: Option<Snapshot>,
    pub reason: Option<String>,
    /// The page opens the Screen Recording sheet when this grows.
    pub permission: u64,
    pub phone_locked: bool,
}

impl MirrorView {
    /// A view without a mirror that keeps the sheet counter.
    fn ended(&mut self, state: MirrorState, reason: Option<String>) {
        *self = MirrorView {
            state,
            reason,
            permission: self.permission,
            ..MirrorView::default()
        };
    }

    fn apply_stats(&mut self, stats: Snapshot) {
        self.phone_locked = stats.phone_locked;
        self.stats = Some(stats);
    }
}

/// An older phone app drops the session on a display request.
pub(super) fn may_ask_displays(info: Option<&AgentInfo>) -> bool {
    info.is_some_and(|i| require_new_app(&i.app_version).is_ok())
}

fn screen_settings_command() -> Command {
    let mut open = Command::new("/usr/bin/open");
    open.arg(SCREEN_SETTINGS_URL);
    open
}

impl App {
    /// After a connect: the display list, if the phone app knows displays.
    pub(super) async fn fetch_displays(&self, client: &Client, info: Option<&AgentInfo>) -> DisplaysView {
        if !may_ask_displays(info) {
            return DisplaysView::default();
        }
        match client.displays().await {
            Ok(list) => DisplaysView::from_list(list),
            Err(e) => {
                self.toast("err", format!("Couldn't list the phone's displays: {}", short(&e)));
                DisplaysView {
                    supported: true,
                    ..DisplaysView::default()
                }
            }
        }
    }

    /// Whether a mirror runs or is starting.
    fn mirror_taken(&self) -> bool {
        lock(&self.mirror).is_some() || lock(&self.model).busy.contains(BUSY)
    }

    pub(super) async fn mirror_start(self: &Arc<Self>, display_id: u32, fps: u32) {
        if self.mirror_taken() {
            self.toast("err", "A mirror is already running or starting.");
            return;
        }
        if !self.claim_busy(BUSY) {
            return;
        }
        self.start_claimed(display_id, fps).await;
        self.set_busy(BUSY, false);
    }

    fn mirror_failed_to_start(&self, reason: String) {
        self.update(|m| m.mirror.ended(MirrorState::Failed, Some(reason)));
    }

    async fn start_claimed(self: &Arc<Self>, display_id: u32, fps: u32) {
        // A disconnect while starting cancels the start.
        let generation = self.mirror_generation.load(Ordering::SeqCst);
        let Some(client) = lock(&self.client).clone() else {
            return self.mirror_failed_to_start("Not connected.".into());
        };
        let fps = if fps == 60 { 60 } else { 30 };
        // The permission check comes first; it may show the system's prompt.
        let source = tokio::task::spawn_blocking(mirror::screen_source).await;
        let source = match source {
            Ok(Ok(source)) => source,
            Ok(Err(e)) if e.is::<ScreenPermissionDenied>() => {
                return self.update(|m| {
                    m.mirror.permission += 1;
                    m.mirror.ended(MirrorState::Idle, None);
                });
            }
            Ok(Err(e)) => return self.mirror_failed_to_start(short(&e)),
            Err(e) => return self.mirror_failed_to_start(e.to_string()),
        };
        let name = lock(&self.model)
            .displays
            .list
            .iter()
            .find(|d| d.id == display_id)
            .map(|d| d.name.clone());
        self.update(|m| {
            m.mirror.ended(MirrorState::Starting, None);
            m.mirror.display = Some(display_id);
            m.mirror.display_name = name.clone();
            m.mirror.fps = fps;
        });
        let options = MirrorOptions {
            fps,
            bitrate_kbps: None,
        };
        let started = {
            let mut store = self.store.lock().await;
            MirrorSession::start(&client, &mut store, display_id, options, source).await
        };
        // The mirror keeps no strong reference, so the session can be closed.
        drop(client);
        let handle = match started {
            Ok(h) => Arc::new(h),
            Err(e) => return self.mirror_failed_to_start(short(&e)),
        };
        let cancelled = {
            let mut slot = lock(&self.mirror);
            if self.mirror_generation.load(Ordering::SeqCst) == generation {
                *slot = Some(Arc::clone(&handle));
                false
            } else {
                true
            }
        };
        if cancelled {
            handle.stop().await;
            return self.update(|m| m.mirror.ended(MirrorState::Idle, None));
        }
        let config = handle.config();
        self.update(|m| {
            m.mirror.state = MirrorState::Running;
            m.mirror.display_name = Some(handle.display().name.clone());
            m.mirror.size = Some(format!("{}×{}", config.width, config.height));
        });
        let me = Arc::clone(self);
        self.rt.spawn(async move { me.follow_mirror(handle).await });
    }

    /// Puts the readout into the model once a second and the reason there at the end.
    async fn follow_mirror(&self, handle: Arc<MirrorHandle>) {
        let mut stats = handle.stats();
        let mut open = true;
        let ending = loop {
            tokio::select! {
                biased;
                ending = handle.done() => break ending,
                changed = stats.changed(), if open => match changed {
                    Ok(()) => {
                        let snapshot = stats.borrow_and_update().clone();
                        if let Some(s) = snapshot {
                            self.update(|m| {
                                if m.mirror.state == MirrorState::Running {
                                    m.mirror.apply_stats(s);
                                }
                            });
                        }
                    }
                    Err(_) => open = false,
                },
            }
        };
        // Whoever took it out of the slot (Stop, a disconnect) has the say about the view.
        let mine = {
            let mut slot = lock(&self.mirror);
            let mine = slot.as_ref().is_some_and(|h| Arc::ptr_eq(h, &handle));
            if mine {
                *slot = None;
            }
            mine
        };
        if mine {
            self.update(|m| {
                if ending.by_user {
                    m.mirror.ended(MirrorState::Idle, None);
                } else {
                    m.mirror.ended(MirrorState::Stopped, Some(ending.reason));
                }
            });
        }
    }

    /// Passes what concerns the mirror from the control session to it.
    pub(super) fn tell_mirror(&self, event: &ClientEvent) {
        let handle = lock(&self.mirror).clone();
        if let Some(h) = handle {
            h.on_control_event(event);
        }
    }

    /// Take the mirror out and cancel one that is starting. The view is idle, or says
    /// `reason`.
    pub(super) fn take_mirror(&self, reason: Option<&str>) -> Option<Arc<MirrorHandle>> {
        let handle = {
            let mut slot = lock(&self.mirror);
            self.mirror_generation.fetch_add(1, Ordering::SeqCst);
            slot.take()
        };
        if handle.is_some() {
            self.update(|m| match reason {
                Some(r) => m.mirror.ended(MirrorState::Stopped, Some(r.to_string())),
                None => m.mirror.ended(MirrorState::Idle, None),
            });
        }
        handle
    }

    /// Stop the mirror and wait (a few seconds at most) until the phone was told.
    pub(super) async fn stop_mirror(&self, reason: Option<&str>) {
        if let Some(handle) = self.take_mirror(reason) {
            let _ = tokio::time::timeout(STOP_LIMIT, handle.stop()).await;
        }
    }

    /// The window quits: the phone removes the picture before the process ends. Called on the
    /// main thread, outside the runtime.
    pub fn stop_mirror_before_quit(&self) {
        self.rt.block_on(self.stop_mirror(None));
    }

    /// An update waits for a mirror that is still starting, as for a starting bridge.
    pub(super) async fn wait_for_mirror_start(&self) {
        let deadline = Instant::now() + STARTING_WAIT;
        while lock(&self.model).busy.contains(BUSY) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub(super) fn open_screen_settings(&self) {
        if !cfg!(target_os = "macos") {
            return;
        }
        match screen_settings_command().status() {
            Ok(status) if status.success() => {}
            Ok(status) => self.toast("err", format!("Couldn't open System Settings ({status})")),
            Err(e) => self.toast("err", format!("Couldn't open System Settings: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Model, Request, update_blocked};
    use super::*;

    #[test]
    fn mirror_requests_deserialize() {
        let parse = |s: &str| serde_json::from_str::<Request>(s).unwrap();
        assert!(matches!(
            parse(r#"{"action":"mirror_start","display_id":10,"fps":60}"#),
            Request::MirrorStart {
                display_id: 10,
                fps: 60
            }
        ));
        assert!(matches!(parse(r#"{"action":"mirror_stop"}"#), Request::MirrorStop));
        assert!(matches!(
            parse(r#"{"action":"open_screen_settings"}"#),
            Request::OpenScreenSettings
        ));
        // The page cannot choose what is opened.
        let sneaky = parse(r#"{"action":"open_screen_settings","url":"file:///"}"#);
        assert!(matches!(sneaky, Request::OpenScreenSettings));
        let model = serde_json::to_value(Model::default()).unwrap();
        assert_eq!(model["mirror"]["state"], "idle");
        assert_eq!(model["mirror"]["permission"], 0);
        assert_eq!(model["displays"]["supported"], false);
    }

    #[test]
    fn only_the_screen_recording_page_is_opened() {
        let command = screen_settings_command();
        assert_eq!(command.get_program(), "/usr/bin/open");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"]
        );
    }

    #[test]
    fn a_mirror_start_is_refused_once_closing_and_does_not_block_an_update() {
        let mut m = Model::default();
        assert!(m.begin_action(BUSY));
        assert!(!update_blocked(&m.busy), "like a starting bridge: the shutdown waits");
        assert!(m.begin_shutdown());
        m.busy.clear();
        assert!(!m.begin_action(BUSY));
        assert!(!m.busy.contains(BUSY));
    }

    #[test]
    fn displays_are_not_requested_from_an_old_app() {
        let info = |version: &str| AgentInfo {
            agent_id: "a".into(),
            name: "n".into(),
            model: "m".into(),
            os: "o".into(),
            app_version: version.into(),
            proto_version: 1,
            usb_host: true,
        };
        assert!(!may_ask_displays(Some(&info("0.1.0"))));
        assert!(!may_ask_displays(Some(&info("?"))));
        assert!(!may_ask_displays(None), "unknown is old");
        assert!(may_ask_displays(Some(&info("0.2.0"))));
    }

    #[test]
    fn the_view_keeps_the_sheet_counter_and_follows_the_lock() {
        let mut v = MirrorView {
            permission: 2,
            state: MirrorState::Running,
            display: Some(10),
            ..MirrorView::default()
        };
        v.apply_stats(Snapshot {
            phone_locked: true,
            ..Snapshot::default()
        });
        assert!(v.phone_locked && v.stats.is_some());
        v.ended(MirrorState::Stopped, Some("the monitor was disconnected".into()));
        assert_eq!((v.permission, v.display, v.phone_locked), (2, None, false));
        assert!(v.stats.is_none());
        assert_eq!(v.reason.as_deref(), Some("the monitor was disconnected"));
    }
}
