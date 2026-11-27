//! HubBridge agent core.
//!
//! Runs inside the Android app (through `hubbridge-android`, JNI) and in the desktop
//! simulator. The platform layer feeds it the current USB device list with
//! [`Agent::set_devices`]; the core owns the TCP server, pairing, sessions and events.
//!
//! The same listener serves Wi-Fi/LAN clients and `adb forward` clients (those arrive
//! from 127.0.0.1 on the phone). Both must authenticate the same way.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use hubbridge_link::{LinkError, Secret, ServerDecision, server_handshake, server_read_hello};
use hubbridge_proto::{AgentInfo, ErrorCode, Frame, HelloError, Intent, Message, PROTO_VERSION, UsbDeviceInfo};
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

/// How long a PIN shown on the phone stays valid.
pub const PAIRING_TTL: Duration = Duration::from_secs(120);
/// Wrong PINs allowed before the PIN is burned and a new one must be started.
pub const PAIRING_MAX_ATTEMPTS: u8 = 3;
/// Time allowed for hello + Noise handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A session with no traffic for this long is closed (hosts ping every few seconds).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(20);
pub const MAX_SESSIONS: usize = 8;

/// Platform hooks the core calls back into.
pub trait AgentCallbacks: Send + Sync {
    /// A host asked to access device `id`: show the system USB permission dialog.
    fn permission_requested(&self, _id: u32) {}
    /// Sessions, pairing or paired hosts changed: refresh the notification / UI.
    fn status_changed(&self) {}
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Where `agent-state.json` (agent id, paired hosts and their keys) lives.
    pub state_dir: PathBuf,
    pub name: String,
    pub model: String,
    pub os: String,
    pub app_version: String,
    pub usb_host: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub id: u64,
    pub host_id: String,
    pub host_name: String,
    pub peer: String,
    /// Came from 127.0.0.1 on the phone: an `adb forward` connection.
    pub via_adb: bool,
    pub connected_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairedHostStatus {
    pub host_id: String,
    pub name: String,
    pub paired_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub agent_id: String,
    pub name: String,
    pub pairing_active: bool,
    pub pairing_seconds_left: u64,
    pub sessions: Vec<SessionStatus>,
    pub paired_hosts: Vec<PairedHostStatus>,
    pub devices: usize,
}

#[derive(Serialize, Deserialize)]
struct PairedHost {
    name: String,
    key: Secret,
    paired_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct PersistState {
    agent_id: String,
    #[serde(default)]
    hosts: BTreeMap<String, PairedHost>,
}

struct Pairing {
    pin: String,
    expires: Instant,
    attempts: u8,
}

struct Inner {
    cfg: AgentConfig,
    state_path: PathBuf,
    state: Mutex<PersistState>,
    devices: RwLock<BTreeMap<u32, UsbDeviceInfo>>,
    events: broadcast::Sender<Message>,
    pairing: Mutex<Option<Pairing>>,
    sessions: Mutex<BTreeMap<u64, SessionStatus>>,
    next_session: AtomicU64,
    callbacks: RwLock<Option<Arc<dyn AgentCallbacks>>>,
    shutdown: watch::Sender<bool>,
}

/// Cheap to clone; all clones share one agent.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

/// Resolves once shutdown was requested (or the agent is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load_or_create_state(path: &Path) -> anyhow::Result<PersistState> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let st = PersistState {
                agent_id: hubbridge_link::random_id(),
                hosts: BTreeMap::new(),
            };
            save_state(path, &st)?;
            Ok(st)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Write via a temp file + rename; owner-only permissions (the file holds keys).
fn save_state(path: &Path, st: &PersistState) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl Agent {
    pub fn new(cfg: AgentConfig) -> anyhow::Result<Self> {
        let state_path = cfg.state_dir.join("agent-state.json");
        let state = load_or_create_state(&state_path)?;
        let (events, _) = broadcast::channel(256);
        let (shutdown, _) = watch::channel(false);
        Ok(Agent {
            inner: Arc::new(Inner {
                cfg,
                state_path,
                state: Mutex::new(state),
                devices: RwLock::new(BTreeMap::new()),
                events,
                pairing: Mutex::new(None),
                sessions: Mutex::new(BTreeMap::new()),
                next_session: AtomicU64::new(1),
                callbacks: RwLock::new(None),
                shutdown,
            }),
        })
    }

    pub fn set_callbacks(&self, cb: Arc<dyn AgentCallbacks>) {
        *self.inner.callbacks.write().unwrap() = Some(cb);
    }

    pub fn agent_id(&self) -> String {
        self.inner.state.lock().unwrap().agent_id.clone()
    }

    pub fn name(&self) -> &str {
        &self.inner.cfg.name
    }

    pub fn info(&self) -> AgentInfo {
        let c = &self.inner.cfg;
        AgentInfo {
            agent_id: self.agent_id(),
            name: c.name.clone(),
            model: c.model.clone(),
            os: c.os.clone(),
            app_version: c.app_version.clone(),
            proto_version: PROTO_VERSION,
            usb_host: c.usb_host,
        }
    }

    fn notify_status(&self) {
        if let Some(cb) = self.inner.callbacks.read().unwrap().clone() {
            cb.status_changed();
        }
    }

    /// Open a pairing window with a fresh random PIN and return it (show it on screen).
    pub fn start_pairing(&self) -> String {
        self.start_pairing_with_pin(hubbridge_link::random_pin())
    }

    /// Same with a caller-chosen PIN (simulator, tests).
    pub fn start_pairing_with_pin(&self, pin: String) -> String {
        *self.inner.pairing.lock().unwrap() = Some(Pairing {
            pin: pin.clone(),
            expires: Instant::now() + PAIRING_TTL,
            attempts: 0,
        });
        self.notify_status();
        pin
    }

    pub fn cancel_pairing(&self) {
        *self.inner.pairing.lock().unwrap() = None;
        self.notify_status();
    }

    /// Remove a paired host; it has to pair again. Its open sessions stay until they close.
    pub fn forget_host(&self, host_id: &str) -> anyhow::Result<bool> {
        let removed = {
            let mut st = self.inner.state.lock().unwrap();
            let removed = st.hosts.remove(host_id).is_some();
            if removed {
                save_state(&self.inner.state_path, &st)?;
            }
            removed
        };
        self.notify_status();
        Ok(removed)
    }

    /// Replace the device list. Hosts get `DeviceAdded` / `DeviceRemoved` for the difference.
    pub fn set_devices(&self, devices: Vec<UsbDeviceInfo>) {
        let mut events = Vec::new();
        {
            let mut cur = self.inner.devices.write().unwrap();
            let new: BTreeMap<u32, UsbDeviceInfo> = devices.into_iter().map(|d| (d.id, d)).collect();
            for id in cur.keys() {
                if !new.contains_key(id) {
                    events.push(Message::DeviceRemoved { id: *id });
                }
            }
            for (id, d) in &new {
                if cur.get(id) != Some(d) {
                    events.push(Message::DeviceAdded(d.clone()));
                }
            }
            *cur = new;
        }
        for e in events {
            let _ = self.inner.events.send(e);
        }
        self.notify_status();
    }

    pub fn devices(&self) -> Vec<UsbDeviceInfo> {
        self.inner.devices.read().unwrap().values().cloned().collect()
    }

    pub fn status(&self) -> AgentStatus {
        let (pairing_active, left) = match &*self.inner.pairing.lock().unwrap() {
            Some(p) if p.expires > Instant::now() => (true, (p.expires - Instant::now()).as_secs()),
            _ => (false, 0),
        };
        let st = self.inner.state.lock().unwrap();
        AgentStatus {
            agent_id: st.agent_id.clone(),
            name: self.inner.cfg.name.clone(),
            pairing_active,
            pairing_seconds_left: left,
            sessions: self.inner.sessions.lock().unwrap().values().cloned().collect(),
            paired_hosts: st
                .hosts
                .iter()
                .map(|(id, h)| PairedHostStatus {
                    host_id: id.clone(),
                    name: h.name.clone(),
                    paired_at_ms: h.paired_at_ms,
                })
                .collect(),
            devices: self.inner.devices.read().unwrap().len(),
        }
    }

    /// Stop accepting and close all sessions.
    pub fn shutdown(&self) {
        let _ = self.inner.shutdown.send(true);
    }

    /// Accept connections until [`Agent::shutdown`].
    pub async fn serve(&self, listener: TcpListener) -> anyhow::Result<()> {
        let mut stop = self.inner.shutdown.subscribe();
        info!(addr = ?listener.local_addr().ok(), agent_id = %self.agent_id(), "agent listening");
        loop {
            tokio::select! {
                _ = stopped(&mut stop) => return Ok(()),
                res = listener.accept() => {
                    let (stream, peer) = match res {
                        Ok(x) => x,
                        Err(e) => { warn!("accept: {e}"); tokio::time::sleep(Duration::from_millis(200)).await; continue; }
                    };
                    let agent = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = agent.handle_conn(stream, peer).await {
                            match e {
                                LinkError::Closed => debug!(%peer, "connection closed"),
                                e => warn!(%peer, "connection ended: {e}"),
                            }
                        }
                    });
                }
            }
        }
    }

    fn decide(&self, hello: &hubbridge_proto::ClientHello) -> ServerDecision {
        if self.inner.sessions.lock().unwrap().len() >= MAX_SESSIONS {
            return ServerDecision::Reject {
                code: HelloError::Busy,
                message: "too many sessions".into(),
            };
        }
        match &hello.intent {
            Intent::Pair { .. } => {
                let mut guard = self.inner.pairing.lock().unwrap();
                match guard.as_mut() {
                    None => ServerDecision::Reject {
                        code: HelloError::PairingNotActive,
                        message: "tap Pair on the phone to get a PIN".into(),
                    },
                    Some(p) if p.expires <= Instant::now() => {
                        *guard = None;
                        ServerDecision::Reject {
                            code: HelloError::PairingNotActive,
                            message: "the PIN expired".into(),
                        }
                    }
                    Some(p) if p.attempts >= PAIRING_MAX_ATTEMPTS => ServerDecision::Reject {
                        code: HelloError::PairingLocked,
                        message: "too many wrong PINs; start pairing again on the phone".into(),
                    },
                    Some(p) => {
                        p.attempts += 1;
                        ServerDecision::Pair { pin: p.pin.clone() }
                    }
                }
            }
            Intent::Resume => match self.inner.state.lock().unwrap().hosts.get(&hello.host_id) {
                Some(h) => ServerDecision::Resume { key: h.key.clone() },
                None => ServerDecision::Reject {
                    code: HelloError::NotPaired,
                    message: "unknown host: pair first".into(),
                },
            },
        }
    }

    async fn handle_conn(&self, stream: TcpStream, peer: SocketAddr) -> Result<(), LinkError> {
        let _ = stream.set_nodelay(true);
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let agent_id = self.agent_id();
        let name = self.inner.cfg.name.clone();

        let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, server_read_hello(&mut r, &mut w, &agent_id, &name))
            .await
            .map_err(|_| LinkError::Protocol("hello timeout"))??;
        let decision = self.decide(&hello.hello);
        let pairing = matches!(decision, ServerDecision::Pair { .. });
        let res = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            server_handshake(r, w, &hello, &agent_id, &name, decision),
        )
        .await
        .map_err(|_| LinkError::Protocol("handshake timeout"))?;
        let session = match res {
            Ok(s) => s,
            Err(e) => {
                if pairing {
                    warn!(%peer, host = %hello.hello.host_name, "pairing attempt failed");
                    self.notify_status();
                }
                return Err(e);
            }
        };
        let host_id = hello.hello.host_id.clone();
        let host_name = hello.hello.host_name.clone();

        if let Some(key) = session.new_key {
            let mut st = self.inner.state.lock().unwrap();
            st.hosts.insert(
                host_id.clone(),
                PairedHost {
                    name: host_name.clone(),
                    key,
                    paired_at_ms: now_ms(),
                },
            );
            if let Err(e) = save_state(&self.inner.state_path, &st) {
                warn!("cannot save paired host: {e:#}");
            }
            drop(st);
            *self.inner.pairing.lock().unwrap() = None;
            info!(%peer, host = %host_name, "paired");
        }

        let sid = self.inner.next_session.fetch_add(1, Ordering::Relaxed);
        self.inner.sessions.lock().unwrap().insert(
            sid,
            SessionStatus {
                id: sid,
                host_id,
                host_name: host_name.clone(),
                peer: peer.to_string(),
                via_adb: peer.ip().is_loopback(),
                connected_at_ms: now_ms(),
            },
        );
        self.notify_status();
        info!(%peer, host = %host_name, session = sid, "session open");

        let res = self.run_session(session.sender, session.receiver).await;

        self.inner.sessions.lock().unwrap().remove(&sid);
        self.notify_status();
        info!(%peer, session = sid, "session closed");
        res
    }

    async fn run_session<R, W>(
        &self,
        mut tx: hubbridge_link::SecureSender<W>,
        rx: hubbridge_link::SecureReceiver<R>,
    ) -> Result<(), LinkError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin,
    {
        // recv() is not cancel-safe, so it lives in its own task.
        let (in_tx, mut in_rx) = mpsc::channel::<Result<Frame, LinkError>>(64);
        let reader = tokio::spawn(async move {
            let mut rx = rx;
            loop {
                let res = rx.recv().await;
                let end = res.is_err();
                if in_tx.send(res).await.is_err() || end {
                    break;
                }
            }
        });
        let mut events = self.inner.events.subscribe();
        let mut stop = self.inner.shutdown.subscribe();

        let result = loop {
            tokio::select! {
                _ = stopped(&mut stop) => {
                    let _ = tx.send(&Frame { req: 0, msg: Message::Bye }).await;
                    break Ok(());
                }
                incoming = tokio::time::timeout(IDLE_TIMEOUT, in_rx.recv()) => {
                    let frame = match incoming {
                        Err(_) => break Err(LinkError::Protocol("idle timeout")),
                        Ok(None) => break Err(LinkError::Closed),
                        Ok(Some(Err(e))) => break Err(e),
                        Ok(Some(Ok(f))) => f,
                    };
                    match self.respond(frame) {
                        Some(reply) => tx.send(&reply).await?,
                        None => break Ok(()),
                    }
                }
                ev = events.recv() => {
                    let msg = match ev {
                        Ok(m) => m,
                        // Fell behind: resend the whole list.
                        Err(broadcast::error::RecvError::Lagged(_)) => Message::DeviceList { devices: self.devices() },
                        Err(broadcast::error::RecvError::Closed) => break Ok(()),
                    };
                    tx.send(&Frame { req: 0, msg }).await?;
                }
            }
        };
        reader.abort();
        tx.shutdown().await;
        result
    }

    /// Answer one request. `None` means the host said goodbye.
    fn respond(&self, frame: Frame) -> Option<Frame> {
        let req = frame.req;
        let msg = match frame.msg {
            Message::Ping { seq, sent_us } => Message::Pong { seq, sent_us },
            Message::GetAgentInfo => Message::AgentInfo(self.info()),
            Message::ListDevices => Message::DeviceList {
                devices: self.devices(),
            },
            Message::RequestPermission { id } => {
                if self.inner.devices.read().unwrap().contains_key(&id) {
                    if let Some(cb) = self.inner.callbacks.read().unwrap().clone() {
                        cb.permission_requested(id);
                    }
                    Message::PermissionRequested { id }
                } else {
                    Message::Error {
                        code: ErrorCode::UnknownDevice,
                        message: format!("no device {id}"),
                    }
                }
            }
            Message::Bye => return None,
            other => Message::Error {
                code: ErrorCode::Unsupported,
                message: format!("unexpected message {:?}", std::mem::discriminant(&other)),
            },
        };
        Some(Frame { req, msg })
    }
}
