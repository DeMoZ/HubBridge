//! Controller of the window: owns the state shown in the page, the connection to the
//! agent, the adb watcher and the mDNS browser. Runs on a tokio runtime; the page gets
//! the whole [`Model`] as JSON after every change (it is small).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
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
use tokio::task::JoinHandle;

/// Sends a script to the page (the window lives on the main thread).
pub type Push = Arc<dyn Fn(String) + Send + Sync>;

const RTT_HISTORY: usize = 90;

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
    /// A PTY can be offered (macOS, Linux).
    pub pty_supported: bool,
    /// Running actions, e.g. "install:R28M303RHET", "pair", "wifi:R28M303RHET".
    pub busy: BTreeSet<String>,
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
pub struct AgentView {
    pub agent_id: String,
    pub name: String,
    pub addrs: Vec<String>,
    pub online: bool,
    pub paired: bool,
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
    Drag,
}

// ---------------------------------------------------------------------------------------
// Settings of the window (not secrets)
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct Settings {
    /// ro.serialno → "ip:5555".
    #[serde(default)]
    adb_wifi: BTreeMap<String, String>,
    #[serde(default)]
    last_target: Option<Target>,
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
    pub fn new(rt: tokio::runtime::Handle, push: Push) -> Result<Arc<App>> {
        let store = Store::load_default()?;
        let app = Arc::new(App {
            rt,
            push,
            model: Mutex::new(Model {
                version: env!("CARGO_PKG_VERSION"),
                pty_supported: cfg!(unix),
                ..Default::default()
            }),
            store: tokio::sync::Mutex::new(store),
            settings: Mutex::new(Settings::load()),
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
        }
        self.push_model();
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
                    app_version: f.app_version,
                }),
            }
        }
        for p in &mut phones {
            p.transports.sort_by_key(|t| t.network);
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
            Request::Drag => {}
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
        self.set_busy(&key, true);
        let opts = BridgeOptions {
            config: SerialConfig {
                line: Line {
                    baud,
                    ..Line::default()
                },
                flow: Flow::None,
                ..SerialConfig::default()
            },
            pty: pty && cfg!(unix),
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
        self.set_busy(&key, true);
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
        self.set_busy("pair", true);
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
        if let Some(c) = lock(&self.client).take() {
            self.rt.spawn(async move {
                if let Ok(c) = Arc::try_unwrap(c) {
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
            });
            self.refresh_agents().await;

            let reason = self.session_loop(&client).await;
            *lock(&self.client) = None;
            self.stop_all_bridges(&format!("connection to the phone lost: {reason}"))
                .await;
            self.update(|m| {
                m.conn.error = Some(reason);
                m.devices.clear();
            });
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
    fn a_device_id_can_be_reserved_once() {
        let set = Mutex::new(BTreeSet::new());
        let first = StartGuard::reserve(&set, 7).expect("first start");
        assert!(StartGuard::reserve(&set, 7).is_none(), "second start refused");
        assert!(StartGuard::reserve(&set, 8).is_some(), "other devices are independent");
        drop(first);
        assert!(StartGuard::reserve(&set, 7).is_some(), "released when the start ends");
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
