//! Connection to one agent: handshake, request/response, events, RTT.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use hubbridge_link::{ClientAuth, LinkError, SecureReceiver, SecureSender, client_handshake};
use hubbridge_proto::{
    self as proto, AgentInfo, DisplayList, ErrorCode, Frame, MAX_MESSAGE_LEN, Message, Setup, UsbDeviceInfo, UsbStatus,
};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::store::{PairedAgent, Store};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Frames that may wait for the writer task per session (back-pressure beyond that).
const SEND_QUEUE: usize = 64;
/// Overall time `close` gives the writer to send what is queued, `Bye` and the shutdown.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Time the writer task gives the orderly shutdown of the socket's write side.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Something the agent sent on its own, or the end of the connection.
#[derive(Clone, Debug)]
pub enum ClientEvent {
    Message(Message),
    Disconnected(String),
}

pub struct Client {
    pub agent_id: String,
    pub agent_name: String,
    pub addr: SocketAddr,
    /// Queue to the writer task, which alone owns the socket's write half. An `Option` only
    /// so that `close` (which consumes the client) can move it out: it is `Some` whenever
    /// a caller can still reach the client.
    tx: Option<mpsc::Sender<Vec<u8>>>,
    session: Arc<Session>,
    next_req: AtomicU32,
    events: Mutex<Option<mpsc::UnboundedReceiver<ClientEvent>>>,
    reader: JoinHandle<()>,
    writer: Option<JoinHandle<()>>,
}

struct State {
    pending: HashMap<u32, oneshot::Sender<Message>>,
    /// Why the session ended; set once, by the first failure.
    ended: Option<String>,
}

/// State shared by the reader task, the writer task and every caller: the requests waiting
/// for an answer and the one end of the session.
struct Session {
    state: Mutex<State>,
    ended_tx: watch::Sender<bool>,
}

impl Session {
    fn new() -> Arc<Session> {
        Arc::new(Session {
            state: Mutex::new(State {
                pending: HashMap::new(),
                ended: None,
            }),
            ended_tx: watch::channel(false).0,
        })
    }

    /// End the session. The first call wins: it records the reason, fails every pending
    /// request (their senders are dropped) and wakes everyone waiting in `ended()`.
    /// Returns whether this call was the first.
    fn end(&self, reason: &str) -> bool {
        let drained = {
            let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if st.ended.is_some() {
                return false;
            }
            st.ended = Some(reason.to_string());
            std::mem::take(&mut st.pending)
        };
        drop(drained);
        self.ended_tx.send_replace(true);
        true
    }

    fn reason(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ended
            .clone()
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn lost(&self) -> anyhow::Error {
        anyhow!("connection lost: {}", self.reason())
    }

    /// Completes once the session has ended (at once if it already has).
    async fn ended(&self) {
        let mut rx = self.ended_tx.subscribe();
        let _ = rx.wait_for(|ended| *ended).await;
    }
}

/// An agent-side failure with its code (e.g. no USB permission).
#[derive(Debug, thiserror::Error)]
#[error("agent error {code:?}: {message}")]
pub struct AgentError {
    pub code: ErrorCode,
    pub message: String,
}

/// A sent request waiting for its response.
pub struct Pending {
    req: u32,
    rx: oneshot::Receiver<Message>,
    session: Arc<Session>,
}

impl Pending {
    pub async fn wait(self, limit: Duration) -> Result<Message> {
        match tokio::time::timeout(limit, self.rx).await {
            Ok(Ok(Message::Error { code, message })) => Err(AgentError { code, message }.into()),
            Ok(Ok(m)) => Ok(m),
            Ok(Err(_)) => Err(self.session.lost()),
            Err(_) => {
                self.session.state.lock().unwrap().pending.remove(&self.req);
                bail!("request timed out")
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // No orderly close: drop the socket right away and fail the pending requests. The
        // tasks are aborted first so the reader does not report our own end as a disconnect.
        self.reader.abort();
        if let Some(w) = &self.writer {
            w.abort();
        }
        self.session.end("client dropped");
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
                    name: session.agent_name.trim().to_string(),
                    key,
                    last_addr: None,
                    paired_at_ms: now_ms(),
                },
            );
        }
        if let Some(a) = store.data.agents.get_mut(&session.agent_id) {
            a.name = session.agent_name.trim().to_string();
            // Also loopback (simulator, adb forward): mDNS addresses are still tried first,
            // and a stale forward port just fails over to them.
            a.last_addr = Some(addr.to_string());
        }
        store.save()?;

        Ok(Self::start(
            session.sender,
            session.receiver,
            session.agent_id,
            session.agent_name.trim().to_string(),
            addr,
            SEND_QUEUE,
        ))
    }

    /// Start the reader task and the writer task of a session that is already established.
    /// `queue` is the number of frames that may wait for the writer.
    fn start<R, W>(
        sender: SecureSender<W>,
        mut receiver: SecureReceiver<R>,
        agent_id: String,
        agent_name: String,
        addr: SocketAddr,
        queue: usize,
    ) -> Client
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let session = Session::new();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();

        let sess = session.clone();
        let reader = tokio::spawn(async move {
            let _guard = EndOnPanic {
                session: sess.clone(),
                reason: "reader stopped",
                events: Some(ev_tx.clone()),
            };
            let reason = loop {
                let next = tokio::select! {
                    next = receiver.recv() => next,
                    _ = sess.ended() => break sess.reason(),
                };
                match next {
                    Ok(Frame {
                        req: 0,
                        msg: Message::Bye,
                    }) => break "agent closed the session".to_string(),
                    Ok(Frame { req: 0, msg }) => {
                        let _ = ev_tx.send(ClientEvent::Message(msg));
                    }
                    Ok(Frame { req, msg }) => {
                        let waiting = sess.state.lock().unwrap().pending.remove(&req);
                        if let Some(tx) = waiting {
                            let _ = tx.send(msg);
                        }
                    }
                    Err(e) => break e.to_string(),
                }
            };
            // The first failure is the reason (a write error comes before the EOF it causes).
            sess.end(&reason);
            let _ = ev_tx.send(ClientEvent::Disconnected(sess.reason()));
        });

        let (tx, rx) = mpsc::channel(queue);
        let writer = tokio::spawn(write_loop(sender, rx, session.clone()));

        Client {
            agent_id,
            agent_name,
            addr,
            tx: Some(tx),
            session,
            next_req: AtomicU32::new(1),
            events: Mutex::new(Some(ev_rx)),
            reader,
            writer: Some(writer),
        }
    }

    /// Take the event stream (device added/removed, disconnect). Only once.
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<ClientEvent>> {
        self.events.lock().unwrap().take()
    }

    /// Queue a request now; the returned [`Pending`] waits for its response. Requests reach
    /// the agent in the order `submit` calls are accepted (sequential calls from one task
    /// keep their order), so several transfers on one endpoint can be in flight and still
    /// complete in order. `submit` returns once the frame is queued for the writer task, not
    /// when it is on the wire; it waits only while the queue is full.
    ///
    /// Cancel-safe: the only await is the wait for queue space. A dropped `submit` either
    /// queued its whole frame (and returned) or did nothing, and never touches the socket.
    pub async fn submit(&self, msg: Message) -> Result<Pending> {
        let tx = self
            .tx
            .as_ref()
            .expect("the queue is only taken by close, which consumes the client");
        let permit = tokio::select! {
            biased;
            _ = self.session.ended() => return Err(self.session.lost()),
            p = tx.reserve() => p.map_err(|_| self.session.lost())?,
        };
        // Nothing below awaits.
        let mut req = self.next_req.fetch_add(1, Ordering::Relaxed);
        if req == 0 {
            req = self.next_req.fetch_add(1, Ordering::Relaxed);
        }
        let bytes = proto::encode(&Frame { req, msg })?;
        if bytes.len() > MAX_MESSAGE_LEN {
            return Err(LinkError::TooLarge(bytes.len()).into());
        }
        let (otx, rx) = oneshot::channel();
        {
            let mut st = self.session.state.lock().unwrap();
            if st.ended.is_some() {
                drop(st);
                return Err(self.session.lost());
            }
            st.pending.insert(req, otx);
        }
        permit.send(bytes);
        Ok(Pending {
            req,
            rx,
            session: Arc::clone(&self.session),
        })
    }

    /// Queue an unsolicited frame (`req = 0`), e.g. video on a media session. Same queue,
    /// order and cancel safety as [`Client::submit`]; nothing answers it.
    pub async fn send_event(&self, msg: Message) -> Result<()> {
        let tx = self
            .tx
            .as_ref()
            .expect("the queue is only taken by close, which consumes the client");
        let permit = tokio::select! {
            biased;
            _ = self.session.ended() => return Err(self.session.lost()),
            p = tx.reserve() => p.map_err(|_| self.session.lost())?,
        };
        let bytes = proto::encode(&Frame { req: 0, msg })?;
        if bytes.len() > MAX_MESSAGE_LEN {
            return Err(LinkError::TooLarge(bytes.len()).into());
        }
        permit.send(bytes);
        Ok(())
    }

    /// Completes once the session has ended, for whatever reason. Does not borrow the
    /// client, so waiting does not keep it from being closed.
    pub fn closed(&self) -> impl Future<Output = ()> + Send + 'static {
        let session = Arc::clone(&self.session);
        async move { session.ended().await }
    }

    /// The phone's presentation displays; also asks for `DisplaysChanged` events. Only for a
    /// phone app of at least [`proto::MIRROR_MIN_APP_VERSION`]: an older one drops the session.
    pub async fn displays(&self) -> Result<DisplayList> {
        match self.request(Message::GetDisplays).await? {
            Message::Displays(list) => Ok(list),
            other => bail!("unexpected reply: {other:?}"),
        }
    }

    /// Send a request and wait for its response.
    pub async fn request(&self, msg: Message) -> Result<Message> {
        self.submit(msg).await?.wait(REQUEST_TIMEOUT).await
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

    // --- USB data (step 2) -------------------------------------------------------------

    async fn expect_done(&self, msg: Message) -> Result<()> {
        match self.request(msg).await? {
            Message::Done => Ok(()),
            other => bail!("unexpected reply: {other:?}"),
        }
    }

    /// Open a device on the phone for this session (needs USB permission there).
    pub async fn open_device(&self, id: u32) -> Result<()> {
        self.expect_done(Message::OpenDevice { id }).await
    }

    pub async fn close_device(&self, id: u32) -> Result<()> {
        self.expect_done(Message::CloseDevice { id }).await
    }

    /// Claim an interface; `detach` takes it away from the phone's own kernel driver.
    pub async fn claim_interface(&self, id: u32, interface: u8, detach: bool) -> Result<()> {
        self.expect_done(Message::ClaimInterface {
            id,
            interface,
            detach_kernel_driver: detach,
        })
        .await
    }

    pub async fn release_interface(&self, id: u32, interface: u8) -> Result<()> {
        self.expect_done(Message::ReleaseInterface { id, interface }).await
    }

    pub async fn set_alternate(&self, id: u32, interface: u8, alternate: u8) -> Result<()> {
        self.expect_done(Message::SetAlternate {
            id,
            interface,
            alternate,
        })
        .await
    }

    pub async fn clear_halt(&self, id: u32, endpoint: u8) -> Result<()> {
        self.expect_done(Message::ClearHalt { id, endpoint }).await
    }

    pub async fn reset_device(&self, id: u32) -> Result<()> {
        self.expect_done(Message::ResetDevice { id }).await
    }

    /// Control transfer on endpoint 0.
    pub async fn control(&self, id: u32, setup: Setup, data: Vec<u8>, timeout: Duration) -> Result<UsbReply> {
        let p = self
            .submit(Message::Control {
                id,
                setup,
                data,
                timeout_ms: timeout.as_millis() as u32,
            })
            .await?;
        match p.wait(timeout + REQUEST_TIMEOUT).await? {
            Message::ControlDone { status, data } => Ok(UsbReply {
                status,
                actual: data.len() as u32,
                data,
            }),
            other => bail!("unexpected reply: {other:?}"),
        }
    }

    /// Queue a bulk/interrupt transfer; await the returned [`PendingTransfer`] for the result.
    pub async fn submit_transfer(
        &self,
        id: u32,
        endpoint: u8,
        length: u32,
        data: Vec<u8>,
        timeout: Duration,
    ) -> Result<PendingTransfer> {
        let p = self
            .submit(Message::Transfer {
                id,
                endpoint,
                length,
                data,
                timeout_ms: timeout.as_millis() as u32,
            })
            .await?;
        Ok(PendingTransfer {
            pending: p,
            limit: timeout + REQUEST_TIMEOUT,
        })
    }

    /// Bulk/interrupt transfer, waiting for the result.
    pub async fn transfer(
        &self,
        id: u32,
        endpoint: u8,
        length: u32,
        data: Vec<u8>,
        timeout: Duration,
    ) -> Result<UsbReply> {
        self.submit_transfer(id, endpoint, length, data, timeout)
            .await?
            .wait()
            .await
    }

    /// Say goodbye and close: every frame accepted so far is sent, then `Bye`, then the
    /// write side is shut down. Bounded by `CLOSE_TIMEOUT` even when the agent stopped reading.
    pub async fn close(mut self) {
        let deadline = tokio::time::Instant::now() + CLOSE_TIMEOUT;
        if let Some(tx) = self.tx.take() {
            if let Ok(bye) = proto::encode(&Frame {
                req: 0,
                msg: Message::Bye,
            }) {
                let queued = async {
                    tokio::select! {
                        biased;
                        _ = self.session.ended() => {}
                        permit = tx.reserve() => {
                            if let Ok(permit) = permit {
                                permit.send(bye);
                            }
                        }
                    }
                };
                let _ = tokio::time::timeout_at(deadline, queued).await;
            }
            // The writer sees the end of the queue after the last frame.
            drop(tx);
        }
        if let Some(mut writer) = self.writer.take()
            && tokio::time::timeout_at(deadline, &mut writer).await.is_err()
        {
            writer.abort();
        }
        self.reader.abort();
        self.session.end("closed");
    }
}

/// Ends the session when its task panics, so that nothing keeps waiting on a half-alive
/// session (a normal return does nothing).
struct EndOnPanic {
    session: Arc<Session>,
    reason: &'static str,
    /// The reader also reports the end on the event stream.
    events: Option<mpsc::UnboundedSender<ClientEvent>>,
}

impl Drop for EndOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking()
            && self.session.end(self.reason)
            && let Some(ev) = &self.events
        {
            let _ = ev.send(ClientEvent::Disconnected(self.reason.to_string()));
        }
    }
}

/// The writer task: the only owner of the socket's write half. It sends queued frames in
/// order; `send_bytes` runs in the branch body, so `select!` never drops it half way.
async fn write_loop<W>(mut sender: SecureSender<W>, mut rx: mpsc::Receiver<Vec<u8>>, session: Arc<Session>)
where
    W: AsyncWrite + Unpin,
{
    let _guard = EndOnPanic {
        session: session.clone(),
        reason: "writer stopped",
        events: None,
    };
    loop {
        tokio::select! {
            biased;
            _ = session.ended() => break,
            next = rx.recv() => match next {
                Some(bytes) => {
                    if let Err(e) = sender.send_bytes(&bytes).await {
                        session.end(&format!("send failed: {e}"));
                        break;
                    }
                }
                None => break,
            },
        }
    }
    // Frames still queued belong to a dead session (or were all sent): discard them.
    rx.close();
    drop(rx);
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, sender.shutdown()).await;
}

/// Result of a control or bulk/interrupt transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbReply {
    pub status: UsbStatus,
    pub data: Vec<u8>,
    pub actual: u32,
}

impl UsbReply {
    /// The data, or an error naming the failed status.
    pub fn ok(self) -> Result<Vec<u8>> {
        match self.status {
            UsbStatus::Ok => Ok(self.data),
            s => bail!("USB transfer failed: {s:?}"),
        }
    }
}

pub struct PendingTransfer {
    pending: Pending,
    limit: Duration,
}

impl PendingTransfer {
    pub async fn wait(self) -> Result<UsbReply> {
        match self.pending.wait(self.limit).await? {
            Message::TransferDone { status, data, actual } => Ok(UsbReply { status, data, actual }),
            other => bail!("unexpected reply: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll, Waker};

    use hubbridge_link::{ClientAuth, Secret, ServerDecision, client_handshake, server_handshake, server_read_hello};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, duplex, split};
    use tokio::time::timeout;

    use super::*;

    const GENEROUS: Duration = Duration::from_secs(10);

    /// Read side of the in-test agent that can be told to stop reading (returns `Pending`).
    struct PauseRead<R> {
        inner: R,
        gate: Arc<Mutex<(bool, Option<Waker>)>>,
    }

    impl<R: AsyncRead + Unpin> AsyncRead for PauseRead<R> {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            {
                let mut gate = self.gate.lock().unwrap();
                if gate.0 {
                    gate.1 = Some(cx.waker().clone());
                    return Poll::Pending;
                }
            }
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    /// Write side of the host that fails every write once `fail` is set (reads keep working).
    struct FaultyWrite<W> {
        inner: W,
        fail: Arc<AtomicBool>,
        /// Panic in `poll_write` once set.
        panic: Arc<AtomicBool>,
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for FaultyWrite<W> {
        fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            if self.panic.load(Ordering::SeqCst) {
                panic!("injected writer panic");
            }
            if self.fail.load(Ordering::SeqCst) {
                return Poll::Ready(Err(io::Error::other("injected write failure")));
            }
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    enum AgentCmd {
        /// Send `Bye` and drop the connection.
        SayBye,
    }

    /// An agent that records every frame it decodes, answers `Ping` with `Pong` and
    /// everything else with `Done`.
    struct TestAgent {
        frames: Arc<Mutex<Vec<Frame>>>,
        /// How the receive loop ended: "eof" or "error: ..." (None while running).
        outcome: Arc<Mutex<Option<String>>>,
        gate: Arc<Mutex<(bool, Option<Waker>)>>,
        cmd: mpsc::UnboundedSender<AgentCmd>,
    }

    impl TestAgent {
        fn pause(&self) {
            self.gate.lock().unwrap().0 = true;
        }
        fn resume(&self) {
            let mut gate = self.gate.lock().unwrap();
            gate.0 = false;
            if let Some(w) = gate.1.take() {
                w.wake();
            }
        }
        fn frames(&self) -> Vec<Frame> {
            self.frames.lock().unwrap().clone()
        }
        async fn wait_outcome(&self) -> String {
            timeout(GENEROUS, async {
                loop {
                    if let Some(o) = self.outcome.lock().unwrap().clone() {
                        return o;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the agent's receive loop did not end")
        }
    }

    struct Rig {
        client: Client,
        agent: TestAgent,
        fail: Arc<AtomicBool>,
        panic: Arc<AtomicBool>,
    }

    fn test_addr() -> SocketAddr {
        "127.0.0.1:1".parse().unwrap()
    }

    /// A host `Client` over a `duplex(pipe)` to the in-test agent.
    async fn rig(pipe: usize, queue: usize) -> Rig {
        let (host_end, agent_end) = duplex(pipe);
        let (hr, hw) = split(host_end);
        let (ar, aw) = split(agent_end);
        let gate = Arc::new(Mutex::new((false, None)));
        let key = Secret([5; 32]);

        let agent_key = key.clone();
        let agent_gate = gate.clone();
        let agent_handshake = async move {
            let mut r = PauseRead {
                inner: ar,
                gate: agent_gate,
            };
            let mut w = aw;
            let hello = server_read_hello(&mut r, &mut w, "agent-t", "Test Agent")
                .await
                .unwrap();
            server_handshake(
                r,
                w,
                &hello,
                "agent-t",
                "Test Agent",
                ServerDecision::Resume { key: agent_key },
            )
            .await
            .unwrap()
        };
        let fail = Arc::new(AtomicBool::new(false));
        let panic = Arc::new(AtomicBool::new(false));
        let host_handshake = client_handshake(
            hr,
            FaultyWrite {
                inner: hw,
                fail: fail.clone(),
                panic: panic.clone(),
            },
            "host-t",
            "Test Host",
            ClientAuth::resume_with(key),
        );
        let (server, client_session) = tokio::join!(agent_handshake, host_handshake);
        let client_session = client_session.unwrap();

        let frames = Arc::new(Mutex::new(Vec::new()));
        let outcome = Arc::new(Mutex::new(None));
        let (cmd, mut cmd_rx) = mpsc::unbounded_channel();
        let (mut tx, mut rx) = (server.sender, server.receiver);
        let (f, o) = (frames.clone(), outcome.clone());
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    frame = rx.recv() => frame,
                    Some(AgentCmd::SayBye) = cmd_rx.recv() => {
                        let _ = tx.send(&Frame { req: 0, msg: Message::Bye }).await;
                        *o.lock().unwrap() = Some("said bye".into());
                        return;
                    }
                };
                match frame {
                    Ok(frame) => {
                        let reply = match &frame.msg {
                            Message::Ping { seq, sent_us } => Some(Message::Pong {
                                seq: *seq,
                                sent_us: *sent_us,
                            }),
                            Message::Bye => None,
                            _ => Some(Message::Done),
                        };
                        let req = frame.req;
                        f.lock().unwrap().push(frame);
                        if let Some(msg) = reply
                            && tx.send(&Frame { req, msg }).await.is_err()
                        {
                            *o.lock().unwrap() = Some("agent send failed".into());
                            return;
                        }
                    }
                    Err(hubbridge_link::LinkError::Closed) => {
                        *o.lock().unwrap() = Some("eof".into());
                        return;
                    }
                    Err(e) => {
                        *o.lock().unwrap() = Some(format!("error: {e}"));
                        return;
                    }
                }
            }
        });

        let client = Client::start(
            client_session.sender,
            client_session.receiver,
            client_session.agent_id,
            client_session.agent_name,
            test_addr(),
            queue,
        );
        Rig {
            client,
            agent: TestAgent {
                frames,
                outcome,
                gate,
                cmd,
            },
            fail,
            panic,
        }
    }

    fn out_transfer(size: usize) -> Message {
        Message::Transfer {
            id: 999,
            endpoint: 0x01,
            length: size as u32,
            data: vec![0xAB; size],
            timeout_ms: 1000,
        }
    }

    fn lost(e: &anyhow::Error) -> bool {
        format!("{e:#}").contains("connection lost")
    }

    /// Fails the test if `fut` does not finish within `GENEROUS`.
    async fn bounded<F: Future>(fut: F) -> F::Output {
        timeout(GENEROUS, fut).await.expect("test step timed out")
    }

    #[tokio::test]
    async fn smoke_ping_round_trip() {
        let r = rig(1 << 16, 8).await;
        let rtt = bounded(r.client.ping(7)).await.unwrap();
        assert!(rtt < GENEROUS);
        assert_eq!(r.agent.frames().len(), 1);
    }

    #[tokio::test]
    async fn dropped_submit_on_full_pipe_keeps_session() {
        let r = rig(256, 2).await;
        let mut events = r.client.take_events().unwrap();
        r.agent.pause();

        // The writer takes the big frame and gets stuck on the full pipe; two more fill the queue.
        let big = bounded(r.client.submit(out_transfer(200 * 1024))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let q1 = bounded(r.client.submit(Message::Ping { seq: 1, sent_us: 0 }))
            .await
            .unwrap();
        let q2 = bounded(r.client.submit(Message::Ping { seq: 2, sent_us: 0 }))
            .await
            .unwrap();

        // A submit that waits for queue space is dropped by a timeout, another is aborted.
        let dropped = timeout(
            Duration::from_millis(100),
            r.client.submit(Message::Ping { seq: 90, sent_us: 0 }),
        )
        .await;
        assert!(dropped.is_err(), "the queue must be full");
        let client = Arc::new(r.client);
        let c2 = client.clone();
        let task = tokio::spawn(async move { c2.submit(Message::Ping { seq: 91, sent_us: 0 }).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // A caller that keeps waiting for queue space is accepted once the agent reads again.
        let c3 = client.clone();
        let waiter = tokio::spawn(async move { c3.submit(Message::Ping { seq: 92, sent_us: 0 }).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiter.is_finished(), "it waits for queue space");

        r.agent.resume();
        let waiter_pending = bounded(waiter).await.unwrap().expect("the waiting submit is accepted");
        bounded(waiter_pending.wait(GENEROUS))
            .await
            .expect("its request is answered");
        bounded(client.ping(3)).await.unwrap();
        for p in [big, q1, q2] {
            bounded(p.wait(GENEROUS)).await.unwrap();
        }
        let frames = r.agent.frames();
        let reqs: Vec<u32> = frames.iter().map(|f| f.req).collect();
        assert_eq!(
            reqs,
            vec![1, 2, 3, 4, 5],
            "no frame from the dropped or aborted submits"
        );
        let pings: Vec<u32> = frames
            .iter()
            .filter_map(|f| match f.msg {
                Message::Ping { seq, .. } => Some(seq),
                _ => None,
            })
            .collect();
        assert_eq!(
            pings,
            vec![1, 2, 92, 3],
            "the waiting submit comes after the queued frames"
        );
        assert!(
            r.agent.outcome.lock().unwrap().is_none(),
            "the agent's loop must still run"
        );
        assert!(events.try_recv().is_err(), "no Disconnected event");
    }

    #[tokio::test]
    async fn write_error_fails_everyone_promptly() {
        let r = rig(256, 4).await;
        let mut events = r.client.take_events().unwrap();
        r.agent.pause();

        let mut pendings = vec![bounded(r.client.submit(out_transfer(200 * 1024))).await.unwrap()];
        tokio::time::sleep(Duration::from_millis(100)).await;
        for seq in 0..4 {
            pendings.push(
                bounded(r.client.submit(Message::Ping { seq, sent_us: 0 }))
                    .await
                    .unwrap(),
            );
        }
        let client = Arc::new(r.client);
        let waiters: Vec<_> = (10..12)
            .map(|seq| {
                let c = client.clone();
                tokio::spawn(async move { c.submit(Message::Ping { seq, sent_us: 0 }).await.map(|_| ()) })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(waiters.iter().all(|w| !w.is_finished()), "they wait for queue space");

        r.fail.store(true, Ordering::SeqCst);
        r.agent.resume();

        let started = Instant::now();
        for p in pendings {
            let e = timeout(Duration::from_secs(1), p.wait(GENEROUS))
                .await
                .unwrap()
                .unwrap_err();
            assert!(lost(&e), "{e:#}");
        }
        for w in waiters {
            let e = timeout(Duration::from_secs(1), w).await.unwrap().unwrap().unwrap_err();
            assert!(lost(&e), "{e:#}");
        }
        assert!(started.elapsed() < Duration::from_secs(2));

        let e = client.submit(Message::GetAgentInfo).await.err().unwrap();
        assert!(format!("{e:#}").contains("connection lost: send failed"), "{e:#}");

        match bounded(events.recv()).await {
            Some(ClientEvent::Disconnected(reason)) => assert!(reason.contains("send failed"), "{reason}"),
            other => panic!("expected Disconnected, got {other:?}"),
        }
        // The agent side then goes away too: still no second event.
        let _ = r.agent.cmd.send(AgentCmd::SayBye);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(events.try_recv().is_err(), "exactly one Disconnected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_whole_and_ordered() {
        let r = rig(1 << 16, 8).await;
        let client = Arc::new(r.client);
        let sizes = [1usize, 4096, 70 * 1024];
        let tasks: Vec<_> = (0..8u32)
            .map(|task| {
                let c = client.clone();
                tokio::spawn(async move {
                    for seq in 0..50u32 {
                        let size = sizes[seq as usize % sizes.len()];
                        let p = c
                            .submit(Message::Transfer {
                                id: task,
                                endpoint: 1,
                                length: seq,
                                data: vec![task as u8; size],
                                timeout_ms: 1000,
                            })
                            .await
                            .unwrap();
                        p.wait(GENEROUS).await.unwrap();
                    }
                })
            })
            .collect();
        // Encryption in a debug build is slow: allow plenty of time.
        for t in tasks {
            timeout(Duration::from_secs(120), t)
                .await
                .expect("callers did not finish")
                .unwrap();
        }
        let frames = r.agent.frames();
        assert_eq!(frames.len(), 400);
        let mut last_seq: HashMap<u32, i64> = HashMap::new();
        for f in &frames {
            let Message::Transfer { id, length, data, .. } = &f.msg else {
                panic!("unexpected {:?}", f.msg);
            };
            assert_eq!(data.len(), sizes[*length as usize % sizes.len()]);
            assert!(data.iter().all(|b| *b == *id as u8), "payload intact");
            let prev = last_seq.insert(*id, *length as i64).unwrap_or(-1);
            assert!(prev < *length as i64, "task {id} out of order");
        }
    }

    #[tokio::test]
    async fn reader_end_fails_submits_at_once() {
        let r = rig(256, 2).await;
        let mut events = r.client.take_events().unwrap();
        r.agent.pause();
        let _big = bounded(r.client.submit(out_transfer(200 * 1024))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _q1 = bounded(r.client.submit(Message::Ping { seq: 1, sent_us: 0 }))
            .await
            .unwrap();
        let _q2 = bounded(r.client.submit(Message::Ping { seq: 2, sent_us: 0 }))
            .await
            .unwrap();
        let client = Arc::new(r.client);
        let c2 = client.clone();
        let waiter = tokio::spawn(async move { c2.submit(Message::Ping { seq: 3, sent_us: 0 }).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiter.is_finished());

        let _ = r.agent.cmd.send(AgentCmd::SayBye);
        let e = timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(lost(&e), "{e:#}");
        let started = Instant::now();
        let e = client.submit(Message::GetAgentInfo).await.err().unwrap();
        assert!(lost(&e), "{e:#}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "far below the 10 s request timeout"
        );
        assert!(matches!(
            bounded(events.recv()).await,
            Some(ClientEvent::Disconnected(_))
        ));
    }

    #[tokio::test]
    async fn oversized_message_refused_locally() {
        let r = rig(1 << 16, 4).await;
        let e = r.client.submit(out_transfer(MAX_MESSAGE_LEN)).await.err().unwrap();
        assert!(format!("{e:#}").contains("message too large"), "{e:#}");
        assert!(r.agent.frames().is_empty());
        bounded(r.client.ping(1)).await.unwrap();
        assert_eq!(r.agent.frames().len(), 1);
    }

    #[tokio::test]
    async fn close_sends_queued_then_bye() {
        let r = rig(1 << 16, 16).await;
        let mut pendings = Vec::new();
        for seq in 0..5 {
            pendings.push(
                bounded(r.client.submit(Message::Ping { seq, sent_us: 0 }))
                    .await
                    .unwrap(),
            );
        }
        bounded(r.client.close()).await;
        assert_eq!(r.agent.wait_outcome().await, "eof");
        let frames = r.agent.frames();
        assert_eq!(frames.len(), 6);
        assert!(frames[..5].iter().all(|f| matches!(f.msg, Message::Ping { .. })));
        assert_eq!(frames[5].msg, Message::Bye);
    }

    #[tokio::test]
    async fn close_with_stuck_peer_is_bounded() {
        let r = rig(256, 4).await;
        r.agent.pause();
        let _stuck = bounded(r.client.submit(out_transfer(1 << 20))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        r.client.close().await;
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "close took {:?}",
            started.elapsed()
        );
        // The connection is dropped: once the agent reads again it sees the end, not a Bye.
        r.agent.resume();
        let outcome = r.agent.wait_outcome().await;
        assert!(outcome == "eof" || outcome.starts_with("error"), "{outcome}");
        assert!(r.agent.frames().iter().all(|f| f.msg != Message::Bye));
    }

    #[tokio::test]
    async fn drop_fails_pending() {
        let r = rig(1 << 16, 4).await;
        r.agent.pause();
        let p = bounded(r.client.submit(Message::Ping { seq: 1, sent_us: 0 }))
            .await
            .unwrap();
        drop(r.client);
        let e = timeout(Duration::from_secs(1), p.wait(GENEROUS))
            .await
            .unwrap()
            .unwrap_err();
        assert!(lost(&e), "{e:#}");
    }

    #[tokio::test]
    async fn writer_panic_ends_the_session() {
        let r = rig(256, 4).await;
        let mut events = r.client.take_events().unwrap();
        r.agent.pause();
        let stuck = bounded(r.client.submit(out_transfer(200 * 1024))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let queued = bounded(r.client.submit(Message::Ping { seq: 1, sent_us: 0 }))
            .await
            .unwrap();

        r.panic.store(true, Ordering::SeqCst);
        r.agent.resume();
        for p in [stuck, queued] {
            let e = timeout(Duration::from_secs(2), p.wait(GENEROUS))
                .await
                .unwrap()
                .unwrap_err();
            assert!(format!("{e:#}").contains("connection lost: writer stopped"), "{e:#}");
        }
        let e = r.client.submit(Message::GetAgentInfo).await.err().unwrap();
        assert!(lost(&e), "{e:#}");
        match bounded(events.recv()).await {
            Some(ClientEvent::Disconnected(reason)) => assert_eq!(reason, "writer stopped"),
            other => panic!("expected Disconnected, got {other:?}"),
        }
        let _ = r.agent.cmd.send(AgentCmd::SayBye);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(events.try_recv().is_err(), "exactly one Disconnected");
    }
}
