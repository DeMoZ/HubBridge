//! Connection to one agent: handshake, request/response, events, RTT.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use hubbridge_link::{ClientAuth, LinkError, SecureSender, client_handshake};
use hubbridge_proto::{AgentInfo, Frame, Message, UsbDeviceInfo};
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::store::{PairedAgent, Store};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Something the agent sent on its own, or the end of the connection.
#[derive(Debug)]
pub enum ClientEvent {
    Message(Message),
    Disconnected(String),
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Message>>>>;

pub struct Client {
    pub agent_id: String,
    pub agent_name: String,
    pub addr: SocketAddr,
    writer: tokio::sync::Mutex<SecureSender<OwnedWriteHalf>>,
    pending: Pending,
    next_req: AtomicU32,
    events: Mutex<Option<mpsc::UnboundedReceiver<ClientEvent>>>,
    reader: JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    now_us() / 1000
}

impl Client {
    /// Pair with the PIN shown on the phone and save the new key in `store`.
    /// `addrs` are tried in order until one accepts the TCP connection.
    pub async fn pair(addrs: &[SocketAddr], store: &mut Store, pin: &str) -> Result<Client> {
        Self::open(addrs, store, ClientAuth::Pair { pin: pin.to_string() }).await
    }

    /// Open a session with a key saved earlier.
    pub async fn connect(addrs: &[SocketAddr], store: &mut Store) -> Result<Client> {
        let keys: HashMap<String, _> = store
            .data
            .agents
            .iter()
            .map(|(id, a)| (id.clone(), a.key.clone()))
            .collect();
        let lookup = Box::new(move |id: &str| keys.get(id).cloned());
        Self::open(addrs, store, ClientAuth::Resume { key_for: lookup }).await
    }

    /// First address that accepts a TCP connection. Handshake errors are not retried
    /// on other addresses: they mean the agent answered.
    async fn tcp(addrs: &[SocketAddr]) -> Result<(TcpStream, SocketAddr)> {
        let mut last = anyhow!("no address to connect to");
        for &addr in addrs {
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
                Ok(Ok(s)) => return Ok((s, addr)),
                Ok(Err(e)) => last = anyhow::Error::new(e).context(format!("connect to {addr}")),
                Err(_) => last = anyhow!("connect to {addr}: timed out"),
            }
        }
        Err(last)
    }

    async fn open(addrs: &[SocketAddr], store: &mut Store, auth: ClientAuth) -> Result<Client> {
        let (stream, addr) = Self::tcp(addrs).await?;
        stream.set_nodelay(true).ok();
        let (r, w) = stream.into_split();
        let pairing = matches!(auth, ClientAuth::Pair { .. });
        let session = tokio::time::timeout(
            CONNECT_TIMEOUT * 2,
            client_handshake(BufReader::new(r), w, &store.data.host_id, &store.data.host_name, auth),
        )
        .await
        .map_err(|_| anyhow!("handshake with {addr}: timed out"))?
        .map_err(|e| match e {
            // A non-HubBridge service (or nothing behind an adb forward) usually just closes.
            LinkError::Closed if !pairing => anyhow!("{addr} closed the connection: is the HubBridge agent running?"),
            e => anyhow::Error::new(e),
        })?;

        // Remember what worked.
        if let Some(key) = session.new_key.clone() {
            store.data.agents.insert(
                session.agent_id.clone(),
                PairedAgent {
                    name: session.agent_name.clone(),
                    key,
                    last_addr: None,
                    paired_at_ms: now_ms(),
                },
            );
        }
        if let Some(a) = store.data.agents.get_mut(&session.agent_id) {
            a.name = session.agent_name.clone();
            if !addr.ip().is_loopback() {
                a.last_addr = Some(addr.to_string());
            }
        }
        store.save()?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let mut receiver = session.receiver;
        let pend = pending.clone();
        let reader = tokio::spawn(async move {
            let reason = loop {
                match receiver.recv().await {
                    Ok(Frame {
                        req: 0,
                        msg: Message::Bye,
                    }) => break "agent closed the session".to_string(),
                    Ok(Frame { req: 0, msg }) => {
                        let _ = ev_tx.send(ClientEvent::Message(msg));
                    }
                    Ok(Frame { req, msg }) => {
                        if let Some(tx) = pend.lock().unwrap().remove(&req) {
                            let _ = tx.send(msg);
                        }
                    }
                    Err(e) => break e.to_string(),
                }
            };
            pend.lock().unwrap().clear();
            let _ = ev_tx.send(ClientEvent::Disconnected(reason));
        });

        Ok(Client {
            agent_id: session.agent_id,
            agent_name: session.agent_name,
            addr,
            writer: tokio::sync::Mutex::new(session.sender),
            pending,
            next_req: AtomicU32::new(1),
            events: Mutex::new(Some(ev_rx)),
            reader,
        })
    }

    /// Take the event stream (device added/removed, disconnect). Only once.
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<ClientEvent>> {
        self.events.lock().unwrap().take()
    }

    /// Send a request and wait for its response.
    pub async fn request(&self, msg: Message) -> Result<Message> {
        let mut req = self.next_req.fetch_add(1, Ordering::Relaxed);
        if req == 0 {
            req = self.next_req.fetch_add(1, Ordering::Relaxed);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req, tx);
        let sent = self.writer.lock().await.send(&Frame { req, msg }).await;
        if let Err(e) = sent {
            self.pending.lock().unwrap().remove(&req);
            return Err(e.into());
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Message::Error { code, message })) => bail!("agent error {code:?}: {message}"),
            Ok(Ok(m)) => Ok(m),
            Ok(Err(_)) => bail!("connection lost"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&req);
                bail!("request timed out")
            }
        }
    }

    /// One round trip through the agent.
    pub async fn ping(&self, seq: u32) -> Result<Duration> {
        let start = Instant::now();
        match self.request(Message::Ping { seq, sent_us: now_us() }).await? {
            Message::Pong { seq: s, .. } if s == seq => Ok(start.elapsed()),
            other => bail!("unexpected reply to ping: {other:?}"),
        }
    }

    pub async fn agent_info(&self) -> Result<AgentInfo> {
        match self.request(Message::GetAgentInfo).await? {
            Message::AgentInfo(i) => Ok(i),
            other => bail!("unexpected reply: {other:?}"),
        }
    }

    pub async fn list_devices(&self) -> Result<Vec<UsbDeviceInfo>> {
        match self.request(Message::ListDevices).await? {
            Message::DeviceList { devices } => Ok(devices),
            other => bail!("unexpected reply: {other:?}"),
        }
    }

    pub async fn request_permission(&self, id: u32) -> Result<()> {
        match self.request(Message::RequestPermission { id }).await? {
            Message::PermissionRequested { .. } => Ok(()),
            other => bail!("unexpected reply: {other:?}"),
        }
    }

    /// Say goodbye and close.
    pub async fn close(self) {
        let mut w = self.writer.lock().await;
        let _ = w
            .send(&Frame {
                req: 0,
                msg: Message::Bye,
            })
            .await;
        w.shutdown().await;
    }
}
