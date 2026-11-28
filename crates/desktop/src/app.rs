//! Controller of the window: owns the state shown in the page, the connection to the
//! agent, the adb watcher and the mDNS browser. Runs on a tokio runtime; the page gets
//! the whole [`Model`] as JSON after every change (it is small).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use hubbridge_host::discovery::{Browser, DiscoveredAgent};
use hubbridge_host::install::{self, InstallOptions};
use hubbridge_host::serial::bridge::{BridgeHandle, BridgeOptions, SerialBridge};
use hubbridge_host::serial::{self, Flow, Line, SerialConfig};
use hubbridge_host::{Client, ClientEvent, Store, adb};
use hubbridge_link::LinkError;
use hubbridge_proto::{AgentInfo, DEFAULT_PORT, HelloError, Message, UsbDeviceInfo};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinHandle;

use crate::{donations, net, update};

mod mirroring;
use mirroring::{DisplaysView, MirrorSlot, MirrorView};

/// Sends a script to the page (the window lives on the main thread).
pub type Push = Arc<dyn Fn(String) + Send + Sync>;

/// Quits the app: the event loop ends and the process exits.
pub type Exit = Arc<dyn Fn() + Send + Sync>;

const RTT_HISTORY: usize = 90;

/// Automatic update checks: at most one in this many seconds, looked at this often while running.
const CHECK_EVERY_SECS: u64 = 86_400;
const CHECK_TICK: Duration = Duration::from_secs(3600);

// ---------------------------------------------------------------------------------------
// What the page shows
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct Model {
    pub version: &'static str,
    pub adb: AdbView,
    pub agents: Vec<AgentView>,
    pub conn: ConnView,
    pub devices: Vec<UsbDeviceInfo>,
    /// Devices a serial driver fits (from their descriptors).
    pub serial_capable: Vec<SerialCapable>,
    /// Serial bridges, running or stopped (with the reason).
    pub serial: Vec<SerialView>,
    /// A PTY can be offered (macOS).
    pub pty_supported: bool,
    /// Running on macOS (system hints, mirroring).
    pub macos: bool,
    /// Running actions, e.g. "install:R28M303RHET", "pair", "wifi:R28M303RHET".
    pub busy: BTreeSet<String>,
    /// The app is closing for an update: no APK install, pairing or bridge start may begin.
    #[serde(skip)]
    closing: bool,
    pub update: UpdateView,
    pub donate: DonateView,
    /// The phone's displays, while connected.
    pub displays: DisplaysView,
    /// The screen mirror: running, or why it ended.
    pub mirror: MirrorView,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UpdateView {
    /// The packaged app; a development build cannot update itself.
    pub can_update: bool,
    /// The folder of the app can be written to.
    pub writable: bool,
    pub auto_update: bool,
    pub checking: bool,
    pub downloading: bool,
    /// A newer release.
    pub available: Option<AvailableView>,
    /// The newer release has no file for this system.
    pub no_file: bool,
    pub status: Option<UpdateStatus>,
    /// The page opens the update sheet when this grows.
    pub offer: u64,
    /// Started again after an update that could not replace the app.
    pub failed_launch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AvailableView {
    pub version: String,
    pub notes: String,
    pub page: String,
}

/// A line of the update sheet: the page translates `key` and fills in `args`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateStatus {
    /// ok | err | new
    pub kind: &'static str,
    pub key: &'static str,
    pub args: serde_json::Value,
}

impl UpdateStatus {
    fn new(kind: &'static str, key: &'static str, args: serde_json::Value) -> Option<UpdateStatus> {
        Some(UpdateStatus { kind, key, args })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DonateView {
    pub loading: bool,
    /// The latest list could not be loaded: a saved copy is shown.
    pub offline: bool,
    pub info: Option<donations::Info>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SerialCapable {
    pub id: u32,
    pub driver: String,
    pub chip: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SerialView {
    pub id: u32,
    pub driver: String,
    pub chip: Option<String>,
    pub pty: Option<String>,
    pub tcp: Option<String>,
    pub rfc2217: bool,
    /// `115200 8N1`.
    pub line: String,
    pub tx: u64,
    /// Bytes that could not be sent to the device.
    pub tx_failed: u64,
    pub rx: u64,
    pub dropped: u64,
    pub errors: u64,
    pub running: bool,
    /// Why it stopped, or why it could not start.
    pub error: Option<String>,
}

impl Model {
    /// Marks an APK install, a pairing or a bridge start as running; refused once the app closes for an update.
    fn begin_action(&mut self, key: &str) -> bool {
        if !self.closing {
            self.busy.insert(key.to_string());
        }
        !self.closing
    }

    /// The last busy check before an update and the start of the shutdown, in one step.
    fn begin_shutdown(&mut self) -> bool {
        self.closing = !update_blocked(&self.busy);
        self.closing
    }

    fn refresh_agent_links(&mut self) {
        for a in &mut self.agents {
            a.link = agent_link(&a.agent_id, a.online, &self.conn);
        }
    }

    /// Recompute which devices a serial driver fits.
    fn refresh_serial_capable(&mut self) {
        self.serial_capable = self
            .devices
            .iter()
            .filter_map(|d| {
                serial::detect(d, 0).ok().map(|det| SerialCapable {
                    id: d.id,
                    driver: det.kind.name().to_string(),
                    chip: det.chip.clone(),
                })
            })
            .collect();
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct AdbView {
    pub found: bool,
    pub hint: String,
    pub error: Option<String>,
    pub phones: Vec<PhoneView>,
}

/// One phone, grouping its adb transports (USB and Wi-Fi) by `ro.serialno`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PhoneView {
    pub key: String,
    pub model: Option<String>,
    pub transports: Vec<TransportView>,
    /// One entry per link kind (USB, Wi-Fi), whatever number of adb transports stands behind it.
    pub links: Vec<LinkView>,
    /// The transport actions use: a ready USB one, then a ready `ip:port`, then any ready one.
    pub action_serial: Option<String>,
    /// HubBridge version installed on the phone.
    pub app_version: Option<String>,
    /// Saved `ip:5555` for ADB over Wi-Fi.
    pub wifi_addr: Option<String>,
    /// The saved Wi-Fi address refuses: the phone restarted, plug the cable once.
    pub wifi_refused: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TransportView {
    pub serial: String,
    pub state: String,
    pub network: bool,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LinkView {
    /// usb | wifi
    pub kind: &'static str,
    /// Any transport of this kind is `device`.
    pub ready: bool,
    /// `device`, or the state of the first not-ready transport when none is ready.
    pub state: String,
    pub serials: Vec<String>,
    /// The hint of the first not-ready transport of this kind.
    pub hint: Option<String>,
}

/// One link per kind, USB first; a kind without transports is left out.
fn links(transports: &[TransportView]) -> Vec<LinkView> {
    [("usb", false), ("wifi", true)]
        .into_iter()
        .filter_map(|(kind, network)| {
            let of_kind: Vec<&TransportView> = transports.iter().filter(|t| t.network == network).collect();
            let first = of_kind.first()?;
            let ready = of_kind.iter().any(|t| t.state == "device");
            let not_ready = of_kind.iter().find(|t| t.state != "device");
            Some(LinkView {
                kind,
                ready,
                state: if ready {
                    "device".into()
                } else {
                    not_ready.unwrap_or(first).state.clone()
                },
                serials: of_kind.iter().map(|t| t.serial.clone()).collect(),
                hint: not_ready.and_then(|t| t.hint.clone()),
            })
        })
        .collect()
}

/// `host:port`, as opposed to an mDNS service name of wireless debugging.
fn is_ip_port(serial: &str) -> bool {
    serial
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}

fn action_serial(transports: &[TransportView]) -> Option<String> {
    let ready = || transports.iter().filter(|t| t.state == "device");
    ready()
        .find(|t| !t.network)
        .or_else(|| ready().find(|t| is_ip_port(&t.serial)))
        .or_else(|| ready().next())
        .map(|t| t.serial.clone())
}

/// What the "On this network" card says about an agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLink {
    ConnectedWifi,
    ConnectedAdb,
    OnNetwork,
    #[default]
    NotSeen,
}

/// The window's own connection counts first; mDNS only says whether the phone is seen.
fn agent_link(agent_id: &str, seen: bool, conn: &ConnView) -> AgentLink {
    if conn.state == "connected" && conn.agent_id.as_deref() == Some(agent_id) {
        if conn.via.as_deref() == Some("adb") {
            AgentLink::ConnectedAdb
        } else {
            AgentLink::ConnectedWifi
        }
    } else if seen {
        AgentLink::OnNetwork
    } else {
        AgentLink::NotSeen
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentView {
    pub agent_id: String,
    pub name: String,
    pub addrs: Vec<String>,
    /// Seen by the window's mDNS browser (pairing is offered then).
    pub online: bool,
    pub paired: bool,
    /// Follows the connection as well as mDNS (recomputed after every model change).
    pub link: AgentLink,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ConnView {
    /// idle | connecting | connected | reconnecting | needs_pair | error
    pub state: String,
    pub target: Option<Target>,
    pub target_label: Option<String>,
    pub agent_id: Option<String>,
    pub name: Option<String>,
    pub addr: Option<String>,
    /// wifi | adb
    pub via: Option<String>,
    pub info: Option<AgentInfo>,
    pub rtt: Vec<f64>,
    pub error: Option<String>,
    pub since_ms: u64,
}

/// What to connect to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum Target {
    /// Agent id: mDNS addresses, then the last address that worked.
    Agent(String),
    /// adb transport serial: through `adb forward`.
    Adb(String),
    /// Typed address or host name.
    Addr(String),
}

// ---------------------------------------------------------------------------------------
// Requests from the page
// ---------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    Ready,
    Connect {
        target: Target,
    },
    Disconnect,
    Pair {
        target: Target,
        pin: String,
    },
    Forget {
        agent_id: String,
    },
    Grant {
        id: u32,
    },
    Install {
        serial: String,
    },
    AdbWifi {
        serial: String,
    },
    AdbWifiOff {
        key: String,
    },
    AdbPair {
        addr: String,
        code: String,
    },
    AdbConnect {
        addr: String,
    },
    /// Start a serial bridge on a device of the phone.
    SerialStart {
        id: u32,
        baud: u32,
        #[serde(default)]
        pty: bool,
        #[serde(default)]
        tcp_port: Option<u16>,
        #[serde(default)]
        rfc2217: bool,
    },
    SerialStop {
        id: u32,
    },
    UpdateCheck,
    UpdateInstall,
    UpdateSkip,
    /// The release page of the new version, or the list of all releases.
    UpdatePage,
    SetAutoUpdate {
        value: bool,
    },
    /// The Support sheet opened; `lang` is the language of the page.
    DonateOpen {
        lang: String,
    },
    OpenUrl {
        url: String,
    },
    /// Mirror this computer's screen onto a display of the phone.
    MirrorStart {
        display_id: u32,
        fps: u32,
    },
    MirrorStop,
    /// The Screen Recording page of System Settings (a fixed address).
    OpenScreenSettings,
    Drag,
}

// ---------------------------------------------------------------------------------------
// Settings of the window (not secrets)
// ---------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Settings {
    /// ro.serialno → "ip:5555".
    #[serde(default)]
    adb_wifi: BTreeMap<String, String>,
    #[serde(default)]
    last_target: Option<Target>,
    #[serde(default = "yes")]
    auto_update: bool,
    /// Last successful update check (Unix seconds).
    #[serde(default)]
    last_update_check: u64,
    /// Version the automatic check does not offer again.
    #[serde(default)]
    skipped_version: String,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            adb_wifi: BTreeMap::new(),
            last_target: None,
            auto_update: true,
            last_update_check: 0,
            skipped_version: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Rules of updating
// ---------------------------------------------------------------------------------------

fn should_auto_check(s: &Settings, can_update: bool, now: u64) -> bool {
    s.auto_update && can_update && now.saturating_sub(s.last_update_check) >= CHECK_EVERY_SECS
}

/// Whether the update sheet opens by itself for the version `found`.
fn should_offer(manual: bool, skipped: &str, found: &str) -> bool {
    manual || skipped != found
}

/// Installing needs a packaged app in a writable folder and a release with a file for this system.
fn can_install(view: &UpdateView) -> bool {
    view.can_update && view.writable && !view.no_file
}

/// How long the shutdown for an update waits for bridges that are still starting.
const STARTING_WAIT: Duration = Duration::from_secs(30);

async fn wait_for_starts(starting: &Mutex<BTreeSet<u32>>, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !lock(starting).is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// An update waits while the app talks to the phone: an APK install or a pairing.
fn update_blocked(busy: &BTreeSet<String>) -> bool {
    busy.iter().any(|k| k == "pair" || k.starts_with("install:"))
}

/// Shows the answer of an update check; returns the newer release, if there is one.
fn apply_check(
    view: &mut UpdateView,
    result: Result<update::Release, String>,
    current: update::Version,
    manual: bool,
    skipped: &str,
) -> Option<update::Release> {
    view.checking = false;
    let release = match result {
        Ok(r) => r,
        Err(e) => {
            view.status = UpdateStatus::new("err", "updCheckFailed", json!({ "err": e }));
            return None;
        }
    };
    if release.version <= current {
        view.available = None;
        view.no_file = false;
        view.status = UpdateStatus::new("ok", "updLatest", json!({ "v": update::format_version(current) }));
        return None;
    }
    let version = update::format_version(release.version);
    view.available = Some(AvailableView {
        version: version.clone(),
        notes: release.notes.clone(),
        page: release.page_url.clone(),
    });
    view.no_file = release.file.is_none();
    if view.no_file {
        view.status = UpdateStatus::new("new", "updNoFile", json!({ "v": version }));
    } else {
        view.status = UpdateStatus::new("new", "updNew", json!({ "v": version }));
        if should_offer(manual, skipped, &version) {
            view.offer += 1;
        }
    }
    Some(release)
}

fn settings_path() -> Option<PathBuf> {
    hubbridge_host::store::default_dir()
        .ok()
        .map(|d| d.join("desktop.json"))
}

impl Settings {
    fn load() -> Self {
        settings_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Some(p) = settings_path() {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if let Ok(b) = serde_json::to_vec_pretty(self) {
                let _ = std::fs::write(p, b);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct PhoneFacts {
    phone_serial: Option<String>,
    app_version: Option<String>,
}

pub struct App {
    rt: tokio::runtime::Handle,
    push: Push,
    model: Mutex<Model>,
    store: tokio::sync::Mutex<Store>,
    settings: Mutex<Settings>,
    discovered: Mutex<Vec<DiscoveredAgent>>,
    /// Facts per adb transport serial, fetched once when it becomes ready.
    facts: Mutex<HashMap<String, PhoneFacts>>,
    /// Saved Wi-Fi addresses that refused (phone restarted).
    refused: Mutex<BTreeSet<String>>,
    reconnecting_adb: Mutex<BTreeSet<String>>,
    conn_task: Mutex<Option<JoinHandle<()>>>,
    client: Mutex<Option<Arc<Client>>>,
    /// The user pressed Disconnect: no auto-connect until they pick a target.
    manual_idle: Mutex<bool>,
    browser: Mutex<Option<Browser>>,
    /// Running serial bridges by device id.
    bridges: Mutex<HashMap<u32, Arc<BridgeHandle>>>,
    /// Devices whose bridge is being started (the id is taken before the first await).
    starting: Mutex<BTreeSet<u32>>,
    /// The running mirror.
    mirror: Mutex<MirrorSlot>,
    /// Grows when a mirror is stopped from outside; a start made before is then dropped.
    mirror_generation: AtomicU64,
    exit: Exit,
    /// The newer release the last check found.
    release: Mutex<Option<update::Release>>,
    /// The donations file shown now, parsed again for every opening.
    donations: Mutex<Vec<u8>>,
    donate_lang: Mutex<String>,
    first_ready: AtomicBool,
}

/// A device id reserved for a starting bridge; released when dropped.
struct StartGuard<'a> {
    set: &'a Mutex<BTreeSet<u32>>,
    id: u32,
}

impl<'a> StartGuard<'a> {
    /// `None` when a start for this id is already under way.
    fn reserve(set: &'a Mutex<BTreeSet<u32>>, id: u32) -> Option<StartGuard<'a>> {
        lock(set).insert(id).then(|| StartGuard { set, id })
    }
}

impl Drop for StartGuard<'_> {
    fn drop(&mut self) {
        lock(self.set).remove(&self.id);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Errors that a retry will not fix: the user has to pair.
fn needs_pairing(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<LinkError>(),
        Some(LinkError::UnknownAgent(_))
            | Some(LinkError::AuthFailed)
            | Some(LinkError::Rejected {
                code: HelloError::NotPaired,
                ..
            })
    )
}

fn short(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

impl App {
    pub fn new(rt: tokio::runtime::Handle, push: Push, exit: Exit) -> Result<Arc<App>> {
        let store = Store::load_default()?;
        let settings = Settings::load();
        let target = update::install_target();
        let update_view = UpdateView {
            can_update: target.is_some(),
            writable: target.as_deref().is_some_and(update::can_write),
            auto_update: settings.auto_update,
            failed_launch: std::env::args().any(|a| a == "--update-failed"),
            ..Default::default()
        };
        let app = Arc::new(App {
            rt,
            push,
            model: Mutex::new(Model {
                version: env!("CARGO_PKG_VERSION"),
                pty_supported: cfg!(target_os = "macos"),
                macos: cfg!(target_os = "macos"),
                update: update_view,
                ..Default::default()
            }),
            store: tokio::sync::Mutex::new(store),
            settings: Mutex::new(settings),
            discovered: Mutex::new(Vec::new()),
            facts: Mutex::new(HashMap::new()),
            refused: Mutex::new(BTreeSet::new()),
            reconnecting_adb: Mutex::new(BTreeSet::new()),
            conn_task: Mutex::new(None),
            client: Mutex::new(None),
            manual_idle: Mutex::new(false),
            browser: Mutex::new(None),
            bridges: Mutex::new(HashMap::new()),
            starting: Mutex::new(BTreeSet::new()),
            mirror: Mutex::new(MirrorSlot::default()),
            mirror_generation: AtomicU64::new(0),
            exit,
            release: Mutex::new(None),
            donations: Mutex::new(donations::cached()),
            donate_lang: Mutex::new(String::new()),
            first_ready: AtomicBool::new(true),
        });
        lock(&app.model).conn.state = "idle".into();
        app.start_background();
        Ok(app)
    }

    // --- pushing to the page -------------------------------------------------------------

    fn push_model(&self) {
        let json = serde_json::to_string(&*lock(&self.model)).unwrap_or_default();
        (self.push)(format!("window.onState&&window.onState({json});"));
    }

    fn toast(&self, kind: &str, text: impl Into<String>) {
        let text = serde_json::to_string(&text.into()).unwrap_or_default();
        (self.push)(format!("window.onToast&&window.onToast({kind:?},{text});"));
    }

    fn update(&self, f: impl FnOnce(&mut Model)) {
        {
            let mut m = lock(&self.model);
            f(&mut m);
            m.refresh_serial_capable();
            m.refresh_agent_links();
        }
        self.push_model();
    }

    /// `set_busy(key, true)` for what must not start while the app closes for an update.
    fn claim_busy(&self, key: &str) -> bool {
        let mut ok = false;
        self.update(|m| ok = m.begin_action(key));
        if !ok {
            self.toast("err", "The app is restarting to update.");
        }
        ok
    }

    fn set_busy(&self, key: &str, on: bool) {
        self.update(|m| {
            if on {
                m.busy.insert(key.to_string());
            } else {
                m.busy.remove(key);
            }
        });
    }

    // --- background watchers -------------------------------------------------------------

    fn start_background(self: &Arc<Self>) {
        // mDNS: the browser thread calls back on every change.
        let me = Arc::clone(self);
        match Browser::start(move |list| {
            *lock(&me.discovered) = list;
            me.rt.block_on(me.refresh_agents());
        }) {
            Ok(b) => *lock(&self.browser) = Some(b),
            Err(e) => self.toast("err", format!("mDNS browsing failed: {e:#}")),
        }

        let me = Arc::clone(self);
        self.rt.spawn(async move { me.refresh_agents().await });

        let me = Arc::clone(self);
        self.rt.spawn(async move { me.adb_watcher().await });

        // The once-a-day rule also holds while the app keeps running.
        let me = Arc::clone(self);
        self.rt.spawn(async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + CHECK_TICK, CHECK_TICK);
            loop {
                tick.tick().await;
                me.auto_check();
            }
        });

        // Auto-connect a moment after start (lets mDNS find the phone first).
        let me = Arc::clone(self);
        self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            me.auto_connect().await;
        });
    }

    async fn refresh_agents(&self) {
        let discovered = lock(&self.discovered).clone();
        let store = self.store.lock().await;
        let mut views: Vec<AgentView> = Vec::new();
        for a in &discovered {
            views.push(AgentView {
                agent_id: a.agent_id.clone(),
                name: a.name.clone(),
                addrs: a.addrs.iter().map(|x| x.to_string()).collect(),
                online: true,
                paired: store.key_for(&a.agent_id).is_some(),
                link: AgentLink::default(),
            });
        }
        for (id, p) in &store.data.agents {
            if !views.iter().any(|v| &v.agent_id == id) {
                views.push(AgentView {
                    agent_id: id.clone(),
                    name: p.name.clone(),
                    addrs: p.last_addr.iter().cloned().collect(),
                    online: false,
                    paired: true,
                    link: AgentLink::default(),
                });
            }
        }
        drop(store);
        views.sort_by(|a, b| {
            (!a.online, !a.paired, a.name.to_lowercase()).cmp(&(!b.online, !b.paired, b.name.to_lowercase()))
        });
        self.update(|m| m.agents = views);
    }

    async fn auto_connect(self: &Arc<Self>) {
        if *lock(&self.manual_idle) || lock(&self.model).conn.state != "idle" {
            return;
        }
        let last = lock(&self.settings).last_target.clone();
        let target = match last {
            Some(t) => Some(t),
            None => {
                let store = self.store.lock().await;
                (store.data.agents.len() == 1).then(|| Target::Agent(store.data.agents.keys().next().cloned().unwrap()))
            }
        };
        if let Some(t) = target {
            self.connect(t);
        }
    }

    async fn adb_watcher(self: Arc<Self>) {
        let mut tick: u64 = 0;
        loop {
            self.poll_adb(tick.is_multiple_of(3)).await;
            tick += 1;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn poll_adb(self: &Arc<Self>, reconnect_wifi: bool) {
        let found = adb::find_adb().is_some();
        let (list, error) = if found {
            match adb::list().await {
                Ok(l) => (l, None),
                Err(e) => (Vec::new(), Some(short(&e))),
            }
        } else {
            (Vec::new(), None)
        };

        // Learn facts of new ready transports (identity, installed app version).
        for d in list.iter().filter(|d| d.state == "device") {
            if lock(&self.facts).contains_key(&d.serial) {
                continue;
            }
            let phone_serial = adb::phone_serial(&d.serial).await;
            let app_version = install::installed_version(&d.serial).await;
            lock(&self.facts).insert(
                d.serial.clone(),
                PhoneFacts {
                    phone_serial,
                    app_version,
                },
            );
        }
        lock(&self.facts).retain(|s, _| list.iter().any(|d| &d.serial == s && d.state == "device"));

        // Group transports by phone.
        let facts = lock(&self.facts).clone();
        let settings_wifi = lock(&self.settings).adb_wifi.clone();
        let refused = lock(&self.refused).clone();
        let mut phones: Vec<PhoneView> = Vec::new();
        for d in &list {
            let f = facts.get(&d.serial).cloned().unwrap_or_default();
            let key = f.phone_serial.clone().unwrap_or_else(|| d.serial.clone());
            let t = TransportView {
                serial: d.serial.clone(),
                state: d.state.clone(),
                network: d.network,
                hint: (d.state != "device").then(|| adb::state_hint(&d.state).to_string()),
            };
            match phones.iter_mut().find(|p| p.key == key) {
                Some(p) => {
                    p.transports.push(t);
                    p.model = p.model.clone().or(d.model.clone());
                    p.app_version = p.app_version.clone().or(f.app_version);
                }
                None => phones.push(PhoneView {
                    wifi_addr: settings_wifi.get(&key).cloned(),
                    wifi_refused: settings_wifi.get(&key).is_some_and(|a| refused.contains(a)),
                    key,
                    model: d.model.clone(),
                    transports: vec![t],
                    links: Vec::new(),
                    action_serial: None,
                    app_version: f.app_version,
                }),
            }
        }
        for p in &mut phones {
            p.transports.sort_by_key(|t| t.network);
            p.links = links(&p.transports);
            p.action_serial = action_serial(&p.transports);
            // Back on the cable: the refused address may work again after "Wi-Fi ADB".
            if p.transports.iter().any(|t| !t.network && t.state == "device")
                && let Some(a) = &p.wifi_addr
            {
                lock(&self.refused).remove(a);
            }
        }

        let view = AdbView {
            found,
            hint: adb::install_hint().into(),
            error,
            phones: phones.clone(),
        };
        let changed = lock(&self.model).adb != view;
        if changed {
            self.update(|m| m.adb = view);
        }

        // Keep saved ADB-over-Wi-Fi addresses connected (like UniversalKM, every ~6 s).
        if reconnect_wifi && found {
            for addr in settings_wifi.into_values() {
                let present = list.iter().any(|d| d.serial == addr && d.state == "device");
                let busy = lock(&self.reconnecting_adb).contains(&addr) || lock(&self.refused).contains(&addr);
                if present || busy {
                    continue;
                }
                lock(&self.reconnecting_adb).insert(addr.clone());
                let me = Arc::clone(self);
                self.rt.spawn(async move {
                    if let Err(e) = adb::connect(&addr).await
                        && format!("{e:#}").contains("refused")
                    {
                        lock(&me.refused).insert(addr.clone());
                    }
                    lock(&me.reconnecting_adb).remove(&addr);
                });
            }
        }
    }

    // --- requests ------------------------------------------------------------------------

    pub fn handle(self: &Arc<Self>, req: Request) {
        match req {
            Request::Ready => {
                self.push_model();
                if self.first_ready.swap(false, Ordering::Relaxed) {
                    self.auto_check();
                }
            }
            Request::Connect { target } => {
                *lock(&self.manual_idle) = false;
                self.connect(target);
            }
            Request::Disconnect => {
                *lock(&self.manual_idle) = true;
                self.disconnect();
            }
            Request::Pair { target, pin } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move { me.pair(target, pin).await });
            }
            Request::Forget { agent_id } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move {
                    {
                        let mut store = me.store.lock().await;
                        store.data.agents.remove(&agent_id);
                        let _ = store.save();
                    }
                    if lock(&me.model).conn.agent_id.as_deref() == Some(&agent_id) {
                        me.disconnect();
                    }
                    {
                        let mut s = lock(&me.settings);
                        if s.last_target == Some(Target::Agent(agent_id.clone())) {
                            s.last_target = None;
                            s.save();
                        }
                    }
                    me.refresh_agents().await;
                    me.toast("ok", "Forgotten. Remove this computer in the phone app too.");
                });
            }
            Request::Grant { id } => {
                let client = lock(&self.client).clone();
                let me = Arc::clone(self);
                self.rt.spawn(async move {
                    match client {
                        Some(c) => match c.request_permission(id).await {
                            Ok(()) => me.toast("ok", "The phone shows the USB permission dialog: tap Allow there."),
                            Err(e) => me.toast("err", short(&e)),
                        },
                        None => me.toast("err", "Not connected."),
                    }
                });
            }
            Request::Install { serial } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move { me.install(serial).await });
            }
            Request::AdbWifi { serial } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move { me.adb_wifi(serial).await });
            }
            Request::AdbWifiOff { key } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move {
                    let addr = lock(&me.settings).adb_wifi.remove(&key);
                    lock(&me.settings).save();
                    if let Some(a) = addr {
                        let _ = adb::disconnect(&a).await;
                        lock(&me.refused).remove(&a);
                    }
                    me.poll_adb(false).await;
                });
            }
            Request::AdbPair { addr, code } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move { me.adb_pair(addr, code).await });
            }
            Request::AdbConnect { addr } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move {
                    me.set_busy("adbconnect", true);
                    let addr = if addr.contains(':') {
                        addr
                    } else {
                        format!("{addr}:{}", adb::TCPIP_PORT)
                    };
                    match adb::connect(&addr).await {
                        Ok(()) => me.toast("ok", format!("adb connected to {addr}")),
                        Err(e) => me.toast("err", short(&e)),
                    }
                    me.set_busy("adbconnect", false);
                    me.poll_adb(false).await;
                });
            }
            Request::SerialStart {
                id,
                baud,
                pty,
                tcp_port,
                rfc2217,
            } => {
                let me = Arc::clone(self);
                self.rt
                    .spawn(async move { me.serial_start(id, baud, pty, tcp_port, rfc2217).await });
            }
            Request::SerialStop { id } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move { me.stop_bridge(id, None).await });
            }
            Request::UpdateCheck => self.check_updates(true),
            Request::UpdateInstall => self.update_install(),
            Request::UpdateSkip => {
                let version = lock(&self.model).update.available.as_ref().map(|a| a.version.clone());
                if let Some(v) = version {
                    let mut s = lock(&self.settings);
                    s.skipped_version = v;
                    s.save();
                }
            }
            Request::UpdatePage => {
                let page = lock(&self.model).update.available.as_ref().map(|a| a.page.clone());
                let url = page.filter(|u| !u.is_empty()).unwrap_or_else(update::releases_page);
                self.open_url(&url);
            }
            Request::SetAutoUpdate { value } => {
                {
                    let mut s = lock(&self.settings);
                    s.auto_update = value;
                    s.save();
                }
                self.update(|m| m.update.auto_update = value);
            }
            Request::DonateOpen { lang } => self.donate_open(lang),
            Request::OpenUrl { url } => self.open_url(&url),
            Request::MirrorStart { display_id, fps } => {
                let me = Arc::clone(self);
                self.rt.spawn(async move { me.mirror_start(display_id, fps).await });
            }
            Request::MirrorStop => {
                let me = Arc::clone(self);
                self.rt.spawn(async move { me.stop_mirror(None).await });
            }
            Request::OpenScreenSettings => self.open_screen_settings(),
            Request::Drag => {}
        }
    }

    // --- updates -------------------------------------------------------------------------

    fn auto_check(self: &Arc<Self>) {
        let can_update = lock(&self.model).update.can_update;
        let due = should_auto_check(&lock(&self.settings), can_update, now_secs());
        if due {
            self.check_updates(false);
        }
    }

    fn check_updates(self: &Arc<Self>, manual: bool) {
        {
            let mut m = lock(&self.model);
            if m.update.checking || m.update.downloading {
                return;
            }
            m.update.checking = true;
            m.update.status = None;
        }
        self.push_model();
        let me = Arc::clone(self);
        self.rt.spawn(async move {
            let (result, writable) = tokio::task::spawn_blocking(|| {
                let writable = update::install_target().as_deref().is_some_and(update::can_write);
                (update::latest(), writable)
            })
            .await
            .unwrap_or_else(|e| (Err(e.to_string()), false));
            // Saved on success only: an offline start tries again at the next tick.
            let skipped = {
                let mut s = lock(&me.settings);
                if result.is_ok() {
                    s.last_update_check = now_secs();
                    s.save();
                }
                s.skipped_version.clone()
            };
            let succeeded = result.is_ok();
            me.update(|m| {
                m.update.writable = writable;
                let found = apply_check(&mut m.update, result, update::current(), manual, &skipped);
                if succeeded {
                    *lock(&me.release) = found;
                }
            });
        });
    }

    fn update_install(self: &Arc<Self>) {
        let Some(release) = lock(&self.release).clone().filter(|r| r.file.is_some()) else {
            return;
        };
        let blocked = {
            let mut m = lock(&self.model);
            if m.update.checking || m.update.downloading || !can_install(&m.update) {
                return;
            }
            let blocked = update_blocked(&m.busy);
            if blocked {
                m.update.status = UpdateStatus::new("err", "updWaitBusy", json!({}));
            } else {
                m.update.downloading = true;
                let v = update::format_version(release.version);
                m.update.status = UpdateStatus::new("new", "updDownloading", json!({ "v": v }));
            }
            blocked
        };
        self.push_model();
        if blocked {
            return;
        }
        let me = Arc::clone(self);
        self.rt.spawn(async move {
            let source = tokio::task::spawn_blocking(move || update::download(&release))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            let source = match source {
                Ok(s) => s,
                Err(e) => return me.update_stopped("updFailed", json!({ "err": e })),
            };
            let Some(target) = update::install_target() else {
                update::discard(&source);
                return me.update_stopped(
                    "updFailed",
                    json!({ "err": "could not find where the app is installed" }),
                );
            };
            // A pairing or an APK install may have started during the download; from here on none can start.
            if !lock(&me.model).begin_shutdown() {
                update::discard(&source);
                return me.update_stopped("updWaitBusy", json!({}));
            }
            // A bridge that is still starting has to be registered before it can be stopped.
            wait_for_starts(&me.starting, STARTING_WAIT).await;
            me.wait_for_mirror_start().await;
            me.stop_all_bridges("the app is updating").await;
            me.disconnect_and_wait().await;
            match update::start_install(&source, &target) {
                Ok(()) => {
                    me.update(|m| m.update.status = UpdateStatus::new("ok", "updRestarting", json!({})));
                    // The replace script waits for this process to end.
                    (me.exit)();
                }
                Err(e) => {
                    update::discard(&source);
                    me.update_stopped(
                        "updFailed",
                        json!({ "err": format!("could not start the installer: {e}") }),
                    );
                }
            }
        });
    }

    /// The install did not happen and the app keeps running.
    fn update_stopped(&self, key: &'static str, args: serde_json::Value) {
        self.update(|m| {
            m.closing = false;
            m.update.downloading = false;
            m.update.status = UpdateStatus::new("err", key, args);
        });
    }

    /// Disconnect and let the session say goodbye before the app quits.
    async fn disconnect_and_wait(&self) {
        *lock(&self.manual_idle) = true;
        // First, while the session is still there to tell the phone.
        self.stop_mirror(None).await;
        let task = lock(&self.conn_task).take();
        if let Some(h) = task {
            h.abort();
            let _ = h.await;
        }
        let client = lock(&self.client).take();
        if let Some(c) = client.and_then(Arc::into_inner) {
            let _ = tokio::time::timeout(Duration::from_secs(3), c.close()).await;
        }
        self.disconnect();
    }

    // --- donations -----------------------------------------------------------------------

    /// Downloaded again every time the sheet opens; the last copy shows until then.
    fn donate_open(self: &Arc<Self>, lang: String) {
        *lock(&self.donate_lang) = lang.clone();
        let info = donations::info(&lock(&self.donations), &lang);
        let start = {
            let mut m = lock(&self.model);
            let start = !m.donate.loading;
            m.donate.loading = true;
            m.donate.offline = false;
            m.donate.info = Some(info);
            start
        };
        self.push_model();
        if !start {
            return;
        }
        let me = Arc::clone(self);
        self.rt.spawn(async move {
            let fresh = tokio::task::spawn_blocking(donations::download).await.ok().flatten();
            let offline = fresh.is_none();
            if let Some(data) = fresh {
                *lock(&me.donations) = data;
            }
            let lang = lock(&me.donate_lang).clone();
            let info = donations::info(&lock(&me.donations), &lang);
            me.update(|m| {
                m.donate = DonateView {
                    loading: false,
                    offline,
                    info: Some(info),
                };
            });
        });
    }

    fn open_url(&self, url: &str) {
        if let Err(e) = net::open_url(url) {
            self.toast("err", format!("Couldn't open the browser: {e}"));
        }
    }

    // --- serial bridges ------------------------------------------------------------------

    /// What the page shows of a running bridge.
    fn bridge_view(id: u32, b: &BridgeHandle) -> SerialView {
        let i = b.info();
        let s = b.stats();
        SerialView {
            id,
            driver: i.driver.to_string(),
            chip: i.chip,
            pty: i.pty_path,
            tcp: i.tcp_addr.map(|a| a.to_string()),
            rfc2217: i.rfc2217,
            line: i.line.to_string(),
            tx: s.tx,
            tx_failed: s.tx_failed,
            rx: s.rx,
            dropped: s.rx_dropped,
            errors: s.line_errors(),
            running: true,
            error: None,
        }
    }

    fn set_serial_view(&self, view: SerialView) {
        self.update(|m| {
            match m.serial.iter_mut().find(|v| v.id == view.id) {
                Some(v) => *v = view,
                None => m.serial.push(view),
            };
        });
    }

    async fn serial_start(self: &Arc<Self>, id: u32, baud: u32, pty: bool, tcp_port: Option<u16>, rfc2217: bool) {
        let fail = |text: String| {
            self.toast("err", text.clone());
            self.update(|m| {
                let v = SerialView {
                    id,
                    error: Some(text),
                    ..Default::default()
                };
                match m.serial.iter_mut().find(|x| x.id == id) {
                    Some(x) => *x = v,
                    None => m.serial.push(v),
                };
            });
        };
        // Taken before the first await: two quick Start presses must not open the device twice.
        // Refusing a duplicate leaves the running bridge's card alone.
        let Some(_reserved) = StartGuard::reserve(&self.starting, id) else {
            self.toast("err", "This serial bridge is already starting.");
            return;
        };
        if lock(&self.bridges).contains_key(&id) {
            self.toast("err", "This serial bridge is already running.");
            return;
        }
        let Some(client) = lock(&self.client).clone() else {
            return fail("Not connected.".into());
        };
        let Some(info) = lock(&self.model).devices.iter().find(|d| d.id == id).cloned() else {
            return fail("The device is gone.".into());
        };
        if !info.has_permission {
            return fail("No USB permission yet: press Ask phone and tap Allow there.".into());
        }
        if !pty && tcp_port.is_none() {
            return fail("Choose a PTY or a TCP port.".into());
        }
        let key = format!("serial:{id}");
        if !self.claim_busy(&key) {
            return;
        }
        let opts = BridgeOptions {
            config: SerialConfig {
                line: Line {
                    baud,
                    ..Line::default()
                },
                flow: Flow::None,
                ..SerialConfig::default()
            },
            pty: pty && cfg!(target_os = "macos"),
            tcp: tcp_port.map(|p| SocketAddr::from(([127, 0, 0, 1], p))),
            rfc2217,
            ..BridgeOptions::default()
        };
        let res = SerialBridge::start(client, &info, opts).await;
        self.set_busy(&key, false);
        match res {
            Ok(b) => {
                let b = Arc::new(b);
                lock(&self.bridges).insert(id, Arc::clone(&b));
                self.set_serial_view(Self::bridge_view(id, &b));
                let me = Arc::clone(self);
                self.rt.spawn(async move {
                    let reason = b.done().await;
                    // Only when it is still the registered bridge: a user stop did its own update.
                    let current = lock(&me.bridges).get(&id).is_some_and(|x| Arc::ptr_eq(x, &b));
                    if current {
                        me.stop_bridge(id, Some(reason)).await;
                    }
                });
            }
            Err(e) => fail(short(&e)),
        }
    }

    /// Stop the bridge of device `id`; `reason` is shown on the card (none: the user stopped it).
    async fn stop_bridge(&self, id: u32, reason: Option<String>) {
        let Some(b) = lock(&self.bridges).remove(&id) else {
            return;
        };
        b.stop().await;
        let mut view = Self::bridge_view(id, &b);
        view.running = false;
        view.error = reason;
        self.set_serial_view(view);
    }

    async fn stop_all_bridges(&self, reason: &str) {
        let ids: Vec<u32> = lock(&self.bridges).keys().copied().collect();
        for id in ids {
            self.stop_bridge(id, Some(reason.to_string())).await;
        }
    }

    /// Byte counters of the running bridges.
    fn refresh_serial_counters(&self) {
        let running: Vec<(u32, Arc<BridgeHandle>)> =
            lock(&self.bridges).iter().map(|(i, b)| (*i, Arc::clone(b))).collect();
        if running.is_empty() {
            return;
        }
        self.update(|m| {
            for (id, b) in &running {
                if let Some(v) = m.serial.iter_mut().find(|v| v.id == *id && v.running) {
                    let s = b.stats();
                    let line = b.info().line.to_string();
                    (v.tx, v.tx_failed, v.rx, v.dropped, v.errors, v.line) =
                        (s.tx, s.tx_failed, s.rx, s.rx_dropped, s.line_errors(), line);
                }
            }
        });
    }

    async fn install(self: &Arc<Self>, serial: String) {
        let key = format!("install:{serial}");
        if !self.claim_busy(&key) {
            return;
        }
        let opts = InstallOptions {
            serial: Some(serial.clone()),
            apk: None,
            launch: true,
            replace_incompatible: false,
        };
        match install::install(&opts).await {
            Ok(r) => {
                let was = r.previous_version.map(|v| format!(" (was {v})")).unwrap_or_default();
                self.toast(
                    "ok",
                    format!(
                        "HubBridge {}{was} installed on {}. On the phone: Start, then Pair.",
                        r.version.as_deref().unwrap_or("?"),
                        r.device.label()
                    ),
                );
                lock(&self.facts).remove(&serial);
            }
            Err(e) => self.toast("err", short(&e)),
        }
        self.set_busy(&key, false);
        self.poll_adb(false).await;
    }

    async fn adb_wifi(self: &Arc<Self>, serial: String) {
        let key = format!("wifi:{serial}");
        self.set_busy(&key, true);
        let phone = adb::phone_serial(&serial).await.unwrap_or_else(|| serial.clone());
        match adb::enable_wifi(&serial).await {
            Ok(addr) => {
                {
                    let mut s = lock(&self.settings);
                    s.adb_wifi.insert(phone, addr.clone());
                    s.save();
                }
                lock(&self.refused).remove(&addr);
                self.toast(
                    "ok",
                    format!("ADB over Wi-Fi is on ({addr}). You can unplug the cable and put the phone on the hub."),
                );
            }
            Err(e) => self.toast("err", short(&e)),
        }
        self.set_busy(&key, false);
        self.poll_adb(false).await;
    }

    async fn adb_pair(self: &Arc<Self>, addr: String, code: String) {
        self.set_busy("adbpair", true);
        let res = async {
            adb::pair(addr.trim(), code.trim()).await?;
            // Then connect to what wireless debugging announces.
            tokio::time::sleep(Duration::from_millis(800)).await;
            let services = adb::mdns_connect_services().await;
            let host = addr.split(':').next().unwrap_or_default().to_string();
            let mut connected = None;
            for (_, a) in services.iter().filter(|(_, a)| a.starts_with(&format!("{host}:"))) {
                if adb::connect(a).await.is_ok() {
                    connected = Some(a.clone());
                    break;
                }
            }
            Ok::<_, anyhow::Error>(connected)
        }
        .await;
        match res {
            Ok(Some(a)) => self.toast("ok", format!("Paired and connected over wireless debugging ({a}).")),
            Ok(None) => self.toast(
                "ok",
                "Paired. Now connect: enter the IP:port shown under \"Wireless debugging\" (not the pairing port).",
            ),
            Err(e) => self.toast("err", short(&e)),
        }
        self.set_busy("adbpair", false);
        self.poll_adb(false).await;
    }

    async fn pair(self: &Arc<Self>, target: Target, pin: String) {
        if !self.claim_busy("pair") {
            return;
        }
        self.stop_connection();
        let res = async {
            let pin = pin.trim().to_string();
            if pin.len() != 6 || !pin.chars().all(|c| c.is_ascii_digit()) {
                bail!("the PIN is 6 digits");
            }
            let addrs = self.resolve(&target).await?;
            let mut store = self.store.lock().await;
            let client = Client::pair(&addrs, &mut store, &pin).await?;
            drop(store);
            let (id, name) = (client.agent_id.clone(), client.agent_name.clone());
            client.close().await;
            Ok((id, name))
        }
        .await;
        self.set_busy("pair", false);
        match res {
            Ok((id, name)) => {
                self.toast("ok", format!("Paired with {name}."));
                self.refresh_agents().await;
                let next = match target {
                    Target::Adb(s) => Target::Adb(s),
                    _ => Target::Agent(id),
                };
                *lock(&self.manual_idle) = false;
                self.connect(next);
            }
            Err(e) => {
                self.toast("err", short(&e));
                self.update(|m| {
                    m.conn.state = "needs_pair".into();
                    m.conn.target = Some(target.clone());
                    m.conn.error = Some(short(&e));
                });
            }
        }
    }

    async fn resolve(&self, target: &Target) -> Result<Vec<SocketAddr>> {
        match target {
            Target::Adb(serial) => Ok(vec![adb::forward(Some(serial), DEFAULT_PORT).await?]),
            Target::Addr(a) => {
                let store = self.store.lock().await;
                hubbridge_host::resolve(Some(a), &store).await
            }
            Target::Agent(id) => {
                let mut addrs: Vec<SocketAddr> = lock(&self.discovered)
                    .iter()
                    .filter(|a| &a.agent_id == id)
                    .flat_map(|a| a.addrs.clone())
                    .collect();
                let store = self.store.lock().await;
                if let Some(last) = store
                    .data
                    .agents
                    .get(id)
                    .and_then(|a| a.last_addr.as_deref())
                    .and_then(|a| a.parse().ok())
                    && !addrs.contains(&last)
                {
                    addrs.push(last);
                }
                drop(store);
                addrs.sort_by_key(|a| a.is_ipv6());
                if addrs.is_empty() {
                    bail!("the phone is not on the network (is the HubBridge app started?)");
                }
                Ok(addrs)
            }
        }
    }

    async fn target_label(&self, target: &Target) -> String {
        match target {
            Target::Adb(s) => format!("ADB {s}"),
            Target::Addr(a) => a.clone(),
            Target::Agent(id) => {
                let store = self.store.lock().await;
                store
                    .data
                    .agents
                    .get(id)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| id.chars().take(8).collect())
            }
        }
    }

    fn stop_connection(&self) {
        // The mirror first: the phone is told before the session closes (below).
        let mirror = self.take_mirror(None);
        if !lock(&self.bridges).is_empty() {
            // Serial bridges need the session: they end with it.
            let ids: Vec<u32> = lock(&self.bridges).keys().copied().collect();
            let bridges: Vec<_> = ids
                .iter()
                .filter_map(|i| lock(&self.bridges).remove(i).map(|b| (*i, b)))
                .collect();
            self.update(|m| {
                for (id, b) in &bridges {
                    let mut v = Self::bridge_view(*id, b);
                    v.running = false;
                    v.error = Some("the connection to the phone was closed".into());
                    match m.serial.iter_mut().find(|x| x.id == *id) {
                        Some(x) => *x = v,
                        None => m.serial.push(v),
                    }
                }
            });
            self.rt.spawn(async move {
                for (_, b) in bridges {
                    b.stop().await;
                }
            });
        }
        if let Some(h) = lock(&self.conn_task).take() {
            h.abort();
        }
        let client = lock(&self.client).take();
        if mirror.is_some() || client.is_some() {
            self.rt.spawn(async move {
                if let Some(m) = mirror {
                    m.stop().await;
                }
                if let Some(c) = client.and_then(Arc::into_inner) {
                    c.close().await;
                }
            });
        }
    }

    fn disconnect(&self) {
        self.stop_connection();
        self.update(|m| {
            m.conn = ConnView {
                state: "idle".into(),
                ..Default::default()
            };
            m.devices.clear();
            m.displays = DisplaysView::default();
        });
    }

    pub fn connect(self: &Arc<Self>, target: Target) {
        self.stop_connection();
        let me = Arc::clone(self);
        let handle = self.rt.spawn(async move { me.run_connection(target).await });
        *lock(&self.conn_task) = Some(handle);
    }

    async fn run_connection(self: Arc<Self>, target: Target) {
        let label = self.target_label(&target).await;
        let mut backoff = Duration::from_secs(1);
        let mut first = true;
        loop {
            self.update(|m| {
                m.conn.state = if first { "connecting" } else { "reconnecting" }.into();
                m.conn.target = Some(target.clone());
                m.conn.target_label = Some(label.clone());
            });
            let res = async {
                let addrs = self.resolve(&target).await?;
                let mut store = self.store.lock().await;
                Client::connect(&addrs, &mut store).await
            }
            .await;
            let client = match res {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    if needs_pairing(&e) {
                        self.update(|m| {
                            m.conn.state = "needs_pair".into();
                            m.conn.error = Some(short(&e));
                        });
                        return;
                    }
                    self.update(|m| m.conn.error = Some(short(&e)));
                    first = false;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                    continue;
                }
            };
            backoff = Duration::from_secs(1);
            first = false;

            let info = client.agent_info().await.ok();
            let devices = client.list_devices().await.unwrap_or_default();
            let displays = self.fetch_displays(&client, info.as_ref()).await;
            let via = if client.addr.ip().is_loopback() { "adb" } else { "wifi" };
            {
                let mut s = lock(&self.settings);
                s.last_target = Some(match &target {
                    Target::Addr(_) => Target::Agent(client.agent_id.clone()),
                    t => t.clone(),
                });
                s.save();
            }
            *lock(&self.client) = Some(Arc::clone(&client));
            self.update(|m| {
                m.conn.state = "connected".into();
                m.conn.agent_id = Some(client.agent_id.clone());
                m.conn.name = Some(client.agent_name.clone());
                m.conn.addr = Some(client.addr.to_string());
                m.conn.via = Some(via.into());
                m.conn.info = info;
                m.conn.error = None;
                m.conn.since_ms = now_ms();
                m.devices = devices;
                m.displays = displays;
            });
            self.refresh_agents().await;

            let reason = self.session_loop(&client).await;
            // A mirror ends with its connection, and says so; it is not started again.
            self.tell_mirror(&ClientEvent::Disconnected(reason.clone()));
            *lock(&self.client) = None;
            self.stop_all_bridges(&format!("connection to the phone lost: {reason}"))
                .await;
            self.update(|m| {
                // Not connected any more: the cards must stop saying so before the next attempt.
                m.conn.state = "reconnecting".into();
                m.conn.error = Some(reason);
                m.devices.clear();
                m.displays = DisplaysView::default();
            });
            self.refresh_agents().await;
        }
    }

    /// Ping every second, apply device events; returns why the session ended.
    async fn session_loop(&self, client: &Arc<Client>) -> String {
        let Some(mut events) = client.take_events() else {
            return "no event stream".into();
        };
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut seq = 0u32;
        let mut last_ok = Instant::now();
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    seq = seq.wrapping_add(1);
                    match client.ping(seq).await {
                        Ok(rtt) => {
                            last_ok = Instant::now();
                            self.refresh_serial_counters();
                            self.update(|m| {
                                m.conn.rtt.push((rtt.as_secs_f64() * 1000.0 * 100.0).round() / 100.0);
                                let n = m.conn.rtt.len();
                                if n > RTT_HISTORY {
                                    m.conn.rtt.drain(..n - RTT_HISTORY);
                                }
                            });
                        }
                        Err(e) if last_ok.elapsed() > Duration::from_secs(5) => return format!("no answer: {e:#}"),
                        Err(_) => {}
                    }
                }
                ev = events.recv() => match ev {
                    Some(ClientEvent::Message(Message::DeviceAdded(d))) => self.update(|m| {
                        match m.devices.iter_mut().find(|x| x.id == d.id) {
                            Some(x) => *x = d,
                            None => m.devices.push(d),
                        }
                    }),
                    Some(ClientEvent::Message(Message::DeviceRemoved { id })) => {
                        self.stop_bridge(id, Some("the device was unplugged".into())).await;
                        self.update(|m| {
                            m.devices.retain(|x| x.id != id);
                            m.serial.retain(|v| v.id != id);
                        });
                    }
                    Some(ClientEvent::Message(Message::DeviceClosed { id, reason })) => {
                        self.stop_bridge(id, Some(format!("device closed on the phone: {reason}"))).await;
                    }
                    Some(ClientEvent::Message(Message::DeviceList { devices })) => self.update(|m| m.devices = devices),
                    Some(ClientEvent::Message(Message::DisplaysChanged(list))) => {
                        self.update(|m| m.displays = DisplaysView::from_list(list));
                    }
                    Some(ev @ ClientEvent::Message(Message::MirrorStopped { .. })) => self.tell_mirror(&ev),
                    Some(ClientEvent::Message(_)) => {}
                    Some(ClientEvent::Disconnected(r)) => return r,
                    None => return "connection closed".into(),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_requests_deserialize() {
        let r: Request = serde_json::from_str(
            r#"{"action":"serial_start","id":1002,"baud":115200,"pty":true,"tcp_port":7000,"rfc2217":true}"#,
        )
        .unwrap();
        match r {
            Request::SerialStart {
                id,
                baud,
                pty,
                tcp_port,
                rfc2217,
            } => assert_eq!(
                (id, baud, pty, tcp_port, rfc2217),
                (1002, 115_200, true, Some(7000), true)
            ),
            other => panic!("{other:?}"),
        }
        // Optional fields may be left out or null.
        let r: Request =
            serde_json::from_str(r#"{"action":"serial_start","id":1,"baud":9600,"tcp_port":null}"#).unwrap();
        assert!(matches!(
            r,
            Request::SerialStart {
                pty: false,
                tcp_port: None,
                rfc2217: false,
                ..
            }
        ));
        let r: Request = serde_json::from_str(r#"{"action":"serial_stop","id":1002}"#).unwrap();
        assert!(matches!(r, Request::SerialStop { id: 1002 }));
    }

    #[test]
    fn update_and_donate_requests_deserialize() {
        let parse = |s: &str| serde_json::from_str::<Request>(s).unwrap();
        assert!(matches!(parse(r#"{"action":"update_check"}"#), Request::UpdateCheck));
        assert!(matches!(
            parse(r#"{"action":"update_install"}"#),
            Request::UpdateInstall
        ));
        assert!(matches!(parse(r#"{"action":"update_skip"}"#), Request::UpdateSkip));
        assert!(matches!(parse(r#"{"action":"update_page"}"#), Request::UpdatePage));
        assert!(matches!(
            parse(r#"{"action":"set_auto_update","value":false}"#),
            Request::SetAutoUpdate { value: false }
        ));
        assert!(matches!(
            parse(r#"{"action":"donate_open","lang":"ru"}"#),
            Request::DonateOpen { lang } if lang == "ru"
        ));
        assert!(matches!(
            parse(r#"{"action":"open_url","url":"https://x"}"#),
            Request::OpenUrl { url } if url == "https://x"
        ));
        let model = serde_json::to_value(Model::default()).unwrap();
        assert_eq!(model["update"]["offer"], 0);
        assert_eq!(model["donate"]["loading"], false);
    }

    #[test]
    fn settings_defaults_old_files_and_round_trip() {
        assert!(Settings::default().auto_update);
        let old: Settings = serde_json::from_str(r#"{"adb_wifi":{},"last_target":null}"#).unwrap();
        assert!(old.auto_update);
        assert_eq!((old.last_update_check, old.skipped_version.as_str()), (0, ""));
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), Settings::default());
        let s = Settings {
            adb_wifi: BTreeMap::from([("R28M".to_string(), "10.0.0.2:5555".to_string())]),
            last_target: Some(Target::Addr("phone.local".into())),
            auto_update: false,
            last_update_check: 1_700_000_000,
            skipped_version: "0.2.0".into(),
        };
        let back: Settings = serde_json::from_slice(&serde_json::to_vec_pretty(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn auto_check_once_a_day() {
        const NOW: u64 = 1_700_000_000;
        let at = |last_update_check| Settings {
            last_update_check,
            ..Settings::default()
        };
        assert!(should_auto_check(&at(NOW - 25 * 3600), true, NOW));
        assert!(should_auto_check(&at(NOW - 24 * 3600), true, NOW));
        assert!(should_auto_check(&at(0), true, NOW));
        assert!(!should_auto_check(&at(NOW - 3 * 3600), true, NOW));
        assert!(!should_auto_check(&at(NOW + 3600), true, NOW), "clock moved back");
        assert!(!should_auto_check(&at(0), false, NOW), "development build");
        let off = Settings {
            auto_update: false,
            ..Settings::default()
        };
        assert!(!should_auto_check(&off, true, NOW));
    }

    #[test]
    fn skipped_version_is_not_offered_again() {
        assert!(!should_offer(false, "0.2.0", "0.2.0"));
        assert!(should_offer(true, "0.2.0", "0.2.0"));
        assert!(should_offer(false, "0.2.0", "0.3.0"));
        assert!(should_offer(false, "", "0.2.0"));
    }

    #[test]
    fn update_waits_for_the_phone() {
        let busy = |k: &str| BTreeSet::from([k.to_string()]);
        assert!(update_blocked(&busy("install:R28M")));
        assert!(update_blocked(&busy("pair")));
        assert!(!update_blocked(&busy("wifi:R28M")));
        assert!(!update_blocked(&busy("adbpair")));
        assert!(!update_blocked(&BTreeSet::new()));
    }

    #[test]
    fn nothing_new_starts_once_the_app_closes_for_an_update() {
        let mut m = Model::default();
        assert!(m.begin_action("pair"));
        assert!(!m.begin_shutdown(), "a pairing runs");
        assert!(m.begin_action("install:R28M"), "a refused shutdown blocks nothing");
        m.busy.clear();
        // A bridge that is starting does not refuse the update: the shutdown waits for it.
        assert!(m.begin_action("serial:1"));
        assert!(m.begin_shutdown());
        for key in ["install:R28M", "pair", "serial:2"] {
            assert!(!m.begin_action(key), "{key}");
            assert!(!m.busy.contains(key), "{key}");
        }
        // The installer did not start: update_stopped clears the flag.
        m.closing = false;
        assert!(m.begin_action("pair"));
    }

    #[test]
    fn shutdown_waits_for_starting_bridges() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let set = Mutex::new(BTreeSet::new());
            let guard = StartGuard::reserve(&set, 7).unwrap();
            let finish = async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                drop(guard);
            };
            let began = Instant::now();
            tokio::join!(wait_for_starts(&set, Duration::from_secs(20)), finish);
            assert!(began.elapsed() >= Duration::from_millis(250));
            assert!(began.elapsed() < Duration::from_secs(20));

            let _stuck = StartGuard::reserve(&set, 8).unwrap();
            wait_for_starts(&set, Duration::from_millis(150)).await;
        });
    }

    #[test]
    fn only_an_installable_update_is_installed() {
        let view = |can_update, writable, no_file| UpdateView {
            can_update,
            writable,
            no_file,
            ..UpdateView::default()
        };
        assert!(can_install(&view(true, true, false)));
        assert!(!can_install(&view(false, true, false)), "development build");
        assert!(!can_install(&view(true, false, false)), "read-only folder");
        assert!(!can_install(&view(true, true, true)), "no file for this system");
    }

    #[test]
    fn check_results_reach_the_page() {
        let release = |version, with_file: bool| update::Release {
            version,
            file: with_file.then(|| update::Asset {
                url: "https://x/mac.zip".into(),
                sha256: Some("ab".into()),
            }),
            page_url: "https://x/page".into(),
            notes: "notes".into(),
        };
        let mut view = UpdateView {
            checking: true,
            ..UpdateView::default()
        };
        let key = |v: &UpdateView| v.status.as_ref().map(|s| (s.kind, s.key));

        assert!(apply_check(&mut view, Ok(release((0, 2, 0), true)), (0, 1, 0), false, "").is_some());
        assert!(!view.checking);
        assert_eq!(key(&view), Some(("new", "updNew")));
        assert_eq!(view.available.as_ref().unwrap().version, "0.2.0");
        assert_eq!((view.offer, view.no_file), (1, false));

        // A skipped version: shown, not offered; a manual check offers it; a newer one is offered again.
        apply_check(&mut view, Ok(release((0, 2, 0), true)), (0, 1, 0), false, "0.2.0");
        assert_eq!(view.offer, 1);
        assert!(view.available.is_some());
        apply_check(&mut view, Ok(release((0, 2, 0), true)), (0, 1, 0), true, "0.2.0");
        assert_eq!(view.offer, 2);
        apply_check(&mut view, Ok(release((0, 3, 0), true)), (0, 1, 0), false, "0.2.0");
        assert_eq!(view.offer, 3);

        // A failed check keeps what was found before.
        assert!(apply_check(&mut view, Err("offline".into()), (0, 1, 0), true, "").is_none());
        assert_eq!(key(&view), Some(("err", "updCheckFailed")));
        assert_eq!(view.status.as_ref().unwrap().args["err"], "offline");
        assert!(view.available.is_some());

        assert!(apply_check(&mut view, Ok(release((0, 4, 0), false)), (0, 1, 0), false, "").is_some());
        assert_eq!(key(&view), Some(("new", "updNoFile")));
        assert_eq!((view.offer, view.no_file), (3, true));

        for latest in [(0, 1, 0), (0, 0, 9)] {
            assert!(apply_check(&mut view, Ok(release(latest, true)), (0, 1, 0), true, "").is_none());
            assert_eq!(key(&view), Some(("ok", "updLatest")));
            assert_eq!(view.status.as_ref().unwrap().args["v"], "0.1.0");
            assert_eq!((&view.available, view.no_file, view.offer), (&None, false, 3));
        }
    }

    #[test]
    fn a_device_id_can_be_reserved_once() {
        let set = Mutex::new(BTreeSet::new());
        let first = StartGuard::reserve(&set, 7).expect("first start");
        assert!(StartGuard::reserve(&set, 7).is_none(), "second start refused");
        assert!(StartGuard::reserve(&set, 8).is_some(), "other devices are independent");
        drop(first);
        assert!(StartGuard::reserve(&set, 7).is_some(), "released when the start ends");
    }

    fn transport(serial: &str, state: &str) -> TransportView {
        TransportView {
            serial: serial.into(),
            state: state.into(),
            network: adb::is_network_serial(serial),
            hint: (state != "device").then(|| adb::state_hint(state).to_string()),
        }
    }

    const IP: &str = "192.168.1.108:5555";
    const MDNS: &str = "adb-R28M303RHET-LfTN6A._adb-tls-connect._tcp.";
    const USB: &str = "R28M303RHET";

    #[test]
    fn two_network_transports_give_one_ready_wifi_link() {
        let got = links(&[transport(IP, "device"), transport(MDNS, "device")]);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].kind, got[0].ready, got[0].state.as_str()),
            ("wifi", true, "device")
        );
        assert_eq!(got[0].serials, [IP, MDNS]);
        assert_eq!(got[0].hint, None);
    }

    #[test]
    fn one_ready_and_one_offline_give_one_ready_link_with_the_hint() {
        let got = links(&[transport(MDNS, "offline"), transport(IP, "device")]);
        assert_eq!(got.len(), 1);
        assert!(got[0].ready);
        assert_eq!(got[0].state, "device");
        assert_eq!(got[0].hint.as_deref(), Some(adb::state_hint("offline")));
    }

    #[test]
    fn an_unauthorized_usb_phone_gives_one_not_ready_usb_link() {
        let t = [transport(USB, "unauthorized")];
        let got = links(&t);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].kind, got[0].ready, got[0].state.as_str()),
            ("usb", false, "unauthorized")
        );
        assert_eq!(got[0].hint.as_deref(), Some(adb::state_hint("unauthorized")));
        assert_eq!(action_serial(&t), None);
    }

    #[test]
    fn actions_prefer_usb_then_ip_port_then_mdns() {
        let all = [
            transport(MDNS, "device"),
            transport(IP, "device"),
            transport(USB, "device"),
        ];
        assert_eq!(action_serial(&all).as_deref(), Some(USB));
        assert_eq!(action_serial(&all[..2]).as_deref(), Some(IP));
        assert_eq!(action_serial(&all[..1]).as_deref(), Some(MDNS));
        let usb_not_ready = [transport(USB, "offline"), transport(MDNS, "device")];
        assert_eq!(action_serial(&usb_not_ready).as_deref(), Some(MDNS));
    }

    #[test]
    fn agent_link_follows_the_connection_and_mdns() {
        let conn = |state: &str, id: &str, via: &str| ConnView {
            state: state.into(),
            agent_id: Some(id.into()),
            via: Some(via.into()),
            ..ConnView::default()
        };
        // Connected over ADB and not seen by mDNS is connected, never "not seen".
        assert_eq!(
            agent_link("a", false, &conn("connected", "a", "adb")),
            AgentLink::ConnectedAdb
        );
        assert_eq!(
            agent_link("a", true, &conn("connected", "a", "wifi")),
            AgentLink::ConnectedWifi
        );
        assert_eq!(
            agent_link("a", false, &conn("connected", "a", "wifi")),
            AgentLink::ConnectedWifi
        );
        // Another agent's connection, or a connection that is gone, does not count.
        assert_eq!(
            agent_link("a", true, &conn("connected", "b", "adb")),
            AgentLink::OnNetwork
        );
        assert_eq!(
            agent_link("a", true, &conn("reconnecting", "a", "adb")),
            AgentLink::OnNetwork
        );
        assert_eq!(agent_link("a", false, &conn("idle", "a", "adb")), AgentLink::NotSeen);
        assert_eq!(agent_link("a", false, &ConnView::default()), AgentLink::NotSeen);
        assert_eq!(serde_json::to_value(AgentLink::ConnectedAdb).unwrap(), "connected_adb");
        assert_eq!(serde_json::to_value(AgentLink::NotSeen).unwrap(), "not_seen");
    }

    #[test]
    fn serial_capable_follows_the_device_list() {
        use hubbridge_proto::fixtures;
        let mut m = Model {
            devices: vec![
                fixtures::cp2102_uart(1),
                fixtures::flash_drive(2),
                fixtures::ft232r_uart(3),
            ],
            ..Model::default()
        };
        m.refresh_serial_capable();
        let got: Vec<_> = m
            .serial_capable
            .iter()
            .map(|c| (c.id, c.driver.as_str(), c.chip.as_deref()))
            .collect();
        assert_eq!(got, vec![(1, "CP210x", None), (3, "FTDI", Some("FT232R"))]);
    }
}
