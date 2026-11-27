//! TCP front ends: a raw byte stream or RFC 2217, one client at a time.
//!
//! Listens on 127.0.0.1 unless told otherwise (there is no authentication). A second client
//! is accepted and closed at once while one is connected. Data that arrives while nobody is
//! connected is discarded, and a new client starts with an empty receive buffer.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, warn};

use super::port::RxBuffer;
use super::rfc2217::{self as r, Event, Negotiator, Parser, Request};
use super::{Flow, ModemStatus, PortEvent, SerialPort};

/// `7000` means 127.0.0.1:7000; anything else must be an `address:port`.
pub fn parse_listen(s: &str) -> Result<SocketAddr> {
    let s = s.trim();
    if let Ok(port) = s.parse::<u16>() {
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    s.parse::<SocketAddr>()
        .map_err(|_| anyhow!("'{s}' is not a port or an address:port (e.g. 7000 or 0.0.0.0:7000)"))
}

#[derive(Debug, Clone, Default)]
pub struct TcpOptions {
    /// Speak telnet with the COM-PORT-OPTION instead of a raw stream.
    pub rfc2217: bool,
    /// Answer line settings with the current ones, without changing them.
    pub lock_line: bool,
    /// Shown as the server's signature.
    pub label: String,
}

pub struct TcpFront {
    addr: SocketAddr,
    warning: Option<String>,
    task: JoinHandle<()>,
    ended: watch::Receiver<bool>,
}

/// Tells the watchers when its task ends, however it ends (also when aborted).
struct EndGuard(watch::Sender<bool>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

impl TcpFront {
    pub async fn start(port: SerialPort, listen: SocketAddr, opts: TcpOptions) -> Result<TcpFront> {
        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("listen on {listen}"))?;
        let addr = listener.local_addr()?;
        let warning = (!addr.ip().is_loopback()).then(|| {
            format!(
                "listening on {addr}: anyone who can reach this address can use the serial port (no authentication)"
            )
        });
        if let Some(w) = &warning {
            warn!("{w}");
        }
        let (end_tx, ended) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _end = EndGuard(end_tx);
            accept_loop(listener, port, opts).await;
        });
        Ok(TcpFront {
            addr,
            warning,
            task,
            ended,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Set when the listener is reachable from other machines.
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    pub fn stop(&self) {
        self.task.abort();
    }

    /// Resolves when the accept loop has ended (port closed, listener error, stopped).
    pub fn ended(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.ended.clone();
        async move {
            let _ = rx.wait_for(|v| *v).await;
        }
    }
}

impl Drop for TcpFront {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Accept errors that pass: the listener goes on after a short wait.
pub fn accept_error_is_transient(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    if matches!(
        e.kind(),
        ConnectionAborted | ConnectionReset | Interrupted | WouldBlock | TimedOut
    ) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if let Some(code) = e.raw_os_error() {
        // EMFILE / ENFILE (descriptors), ENOBUFS / ENOMEM (memory), ECONNABORTED, EPROTO.
        return matches!(
            code,
            libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM | libc::ECONNABORTED | libc::EPROTO
        );
    }
    #[cfg(windows)]
    if let Some(code) = e.raw_os_error() {
        // WSAEMFILE, WSAENOBUFS.
        return matches!(code, 10024 | 10055);
    }
    false
}

async fn accept_loop(listener: TcpListener, port: SerialPort, opts: TcpOptions) {
    let active = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(Notify::new());
    let rx = port.tap();
    let done = port.done();
    tokio::pin!(done);
    // Dropping the set (when this task is aborted) aborts the client too.
    let mut clients = JoinSet::new();
    loop {
        let idle = !active.load(Ordering::SeqCst);
        tokio::select! {
            acc = listener.accept() => {
                let (stream, peer) = match acc {
                    Ok(c) => c,
                    Err(e) if accept_error_is_transient(&e) => {
                        // Out of descriptors, a client that left before we got to it, a signal:
                        // not the end of the listener.
                        warn!("accept failed, trying again: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(e) => {
                        warn!("accept failed, the TCP front end ends: {e}");
                        return;
                    }
                };
                stream.set_nodelay(true).ok();
                if active.swap(true, Ordering::SeqCst) {
                    debug!(%peer, "second client refused");
                    drop(stream);
                    continue;
                }
                debug!(%peer, "client connected");
                rx.clear();
                let (port, opts, rx) = (port.clone(), opts.clone(), Arc::clone(&rx));
                let (active, ended) = (Arc::clone(&active), Arc::clone(&ended));
                clients.spawn(async move {
                    if opts.rfc2217 {
                        serve_rfc2217(stream, port, rx, opts).await;
                    } else {
                        serve_raw(stream, port, rx).await;
                    }
                    active.store(false, Ordering::SeqCst);
                    ended.notify_one();
                });
            }
            // Nobody is connected: do not let the receive buffer fill up.
            chunk = rx.recv(), if idle => {
                if chunk.is_none() {
                    return;
                }
            }
            _ = ended.notified() => {}
            _ = &mut done => return,
        }
    }
}

async fn serve_raw(stream: TcpStream, port: SerialPort, rx: Arc<RxBuffer>) {
    let (mut rd, mut wr) = stream.into_split();
    let to_device = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if port.write(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    };
    let to_client = async {
        while let Some(chunk) = rx.recv().await {
            if wr.write_all(&chunk).await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        _ = to_device => {}
        _ = to_client => {}
    }
}

// --- RFC 2217 -------------------------------------------------------------------------------

struct Masks {
    line: AtomicU8,
    modem: AtomicU8,
}

struct Session {
    port: SerialPort,
    opts: TcpOptions,
    out: mpsc::UnboundedSender<Vec<u8>>,
    suspend: watch::Sender<bool>,
    masks: Arc<Masks>,
    /// This client has asked the port for modem status (counted while its mask is not 0).
    demand: bool,
}

async fn serve_rfc2217(stream: TcpStream, port: SerialPort, rx: Arc<RxBuffer>, opts: TcpOptions) {
    let (mut rd, mut wr) = stream.into_split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (suspend, mut suspend_rx) = watch::channel(false);
    let masks = Arc::new(Masks {
        line: AtomicU8::new(0),
        modem: AtomicU8::new(255),
    });

    // One writer: replies and notifications first, device data unless suspended.
    let writer = tokio::spawn(async move {
        loop {
            let suspended = *suspend_rx.borrow();
            tokio::select! {
                biased;
                c = out_rx.recv() => {
                    let Some(c) = c else { return };
                    if wr.write_all(&c).await.is_err() {
                        return;
                    }
                }
                chunk = rx.recv(), if !suspended => {
                    let Some(chunk) = chunk else { return };
                    if wr.write_all(&r::escape(&chunk)).await.is_err() {
                        return;
                    }
                }
                changed = suspend_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    });

    let notifier = tokio::spawn(notify_task(
        port.subscribe(),
        port.modem_status(),
        Arc::clone(&masks),
        out.clone(),
    ));

    let (mut negotiator, hello) = Negotiator::new();
    let _ = out.send(hello);
    let mut session = Session {
        port: port.clone(),
        opts,
        out,
        suspend,
        masks,
        demand: true,
    };
    // The default modem-state mask is not 0: this client wants modem status.
    port.want_modem_status(true);

    let mut parser = Parser::new();
    let mut buf = vec![0u8; 16 * 1024];
    'conn: loop {
        let n = match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for ev in parser.feed(&buf[..n]) {
            match ev {
                Event::Data(d) => {
                    if port.write(&d).await.is_err() {
                        break 'conn;
                    }
                }
                Event::Negotiate { verb, option } => {
                    let answer = negotiator.on_negotiate(verb, option);
                    if !answer.is_empty() {
                        let _ = session.out.send(answer);
                    }
                }
                Event::Sub {
                    option: r::OPT_COM_PORT,
                    data,
                } => {
                    if let Some(req) = r::parse_request(&data) {
                        session.handle(req).await;
                    }
                }
                Event::Sub { .. } => {}
            }
        }
    }
    if session.demand {
        port.want_modem_status(false);
    }
    notifier.abort();
    writer.abort();
}

/// Modem and line state notifications, filtered by the client's masks.
async fn notify_task(
    mut events: broadcast::Receiver<PortEvent>,
    initial: ModemStatus,
    masks: Arc<Masks>,
    out: mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut prev = initial;
    loop {
        match events.recv().await {
            Ok(PortEvent::Modem(m)) => {
                let byte = r::modem_state_byte(prev, m);
                let changed = (byte & 0x0f) | (r::modem_state_byte(prev, prev) ^ r::modem_state_byte(m, m)) & 0xf0;
                prev = m;
                // The notification carries the state and the client's mask (state AND mask).
                let mask = masks.modem.load(Ordering::Relaxed);
                if changed & mask != 0 && out.send(r::notify_modemstate(byte & mask)).is_err() {
                    return;
                }
            }
            Ok(PortEvent::LineError(e)) => {
                let byte = r::line_state_byte(e);
                let mask = masks.line.load(Ordering::Relaxed);
                if byte & mask != 0 && out.send(r::notify_linestate(byte & mask)).is_err() {
                    return;
                }
            }
            Ok(PortEvent::OutFailed { .. }) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

impl Session {
    fn send(&self, bytes: Vec<u8>) {
        let _ = self.out.send(bytes);
    }

    /// Apply a line setting and answer with what is in effect afterwards.
    async fn set_line(&self, change: impl FnOnce(&mut super::Line)) {
        if self.opts.lock_line {
            return;
        }
        let mut line = self.port.line();
        change(&mut line);
        if line == self.port.line() {
            return;
        }
        if let Err(e) = self.port.set_line(line).await {
            warn!("RFC 2217 line change refused: {e:#}");
        }
    }

    async fn handle(&mut self, req: Request) {
        match req {
            Request::Signature(_) => {
                self.send(r::reply_signature(&format!("HubBridge {}", self.opts.label)));
            }
            Request::SetBaudrate(baud) => {
                if baud != 0 {
                    self.set_line(|l| l.baud = baud).await;
                }
                self.send(r::reply_baudrate(self.port.line().baud));
            }
            Request::SetDatasize(n) => {
                // 0 asks; 5 to 8 are the sizes a UART has, anything else is ignored.
                if (5..=8).contains(&n) {
                    self.set_line(|l| l.data_bits = n).await;
                }
                self.send(r::reply_byte(r::CMD_SET_DATASIZE, self.port.line().data_bits));
            }
            Request::SetParity(c) => {
                if let Some(p) = r::parity_from_code(c) {
                    self.set_line(|l| l.parity = p).await;
                }
                self.send(r::reply_byte(
                    r::CMD_SET_PARITY,
                    r::parity_code(self.port.line().parity),
                ));
            }
            Request::SetStopsize(c) => {
                if let Some(s) = r::stopsize_from_code(c) {
                    self.set_line(|l| l.stop_bits = s).await;
                }
                self.send(r::reply_byte(
                    r::CMD_SET_STOPSIZE,
                    r::stopsize_code(self.port.line().stop_bits),
                ));
            }
            Request::SetControl(v) => {
                let value = self.set_control(v).await;
                self.send(r::reply_byte(r::CMD_SET_CONTROL, value));
            }
            Request::FlowSuspend => {
                let _ = self.suspend.send(true);
            }
            Request::FlowResume => {
                let _ = self.suspend.send(false);
            }
            Request::SetLinestateMask(m) => {
                self.masks.line.store(m, Ordering::Relaxed);
                self.send(r::reply_byte(r::CMD_SET_LINESTATE_MASK, m));
            }
            Request::SetModemstateMask(m) => {
                self.masks.modem.store(m, Ordering::Relaxed);
                self.send(r::reply_byte(r::CMD_SET_MODEMSTATE_MASK, m));
                let want = m != 0;
                if want != self.demand {
                    self.port.want_modem_status(want);
                    self.demand = want;
                }
                if want {
                    // Tell the client the current lines once.
                    let now = self.port.modem_status();
                    self.send(r::notify_modemstate(r::modem_state_byte(now, now)));
                }
            }
            Request::PurgeData(v) => {
                let (rx, tx) = (v == 1 || v == 3, v == 2 || v == 3);
                if let Err(e) = self.port.purge(rx, tx).await {
                    warn!("RFC 2217 purge failed: {e:#}");
                }
                self.send(r::reply_byte(r::CMD_PURGE_DATA, v));
            }
            Request::Unknown(_) => {}
        }
    }

    /// SET-CONTROL: returns the value to answer with (the state in effect).
    async fn set_control(&mut self, v: u8) -> u8 {
        let p = &self.port;
        let inbound = |f: Flow| if f == Flow::RtsCts { 16 } else { 14 };
        match v {
            1 | 3 => {
                if !self.opts.lock_line {
                    let want = if v == 3 { Flow::RtsCts } else { Flow::None };
                    if let Err(e) = p.set_flow(want).await {
                        warn!("RFC 2217 flow control change refused: {e:#}");
                    }
                }
                r::flow_code(p.flow())
            }
            5 | 6 => {
                if let Err(e) = p.set_break(v == 5).await {
                    warn!("RFC 2217 break refused: {e:#}");
                }
                if p.break_on() { 5 } else { 6 }
            }
            4 => {
                if p.break_on() {
                    5
                } else {
                    6
                }
            }
            7 => {
                if p.dtr() {
                    8
                } else {
                    9
                }
            }
            8 | 9 => {
                if let Err(e) = p.set_dtr(v == 8).await {
                    warn!("RFC 2217 DTR change refused: {e:#}");
                }
                if p.dtr() { 8 } else { 9 }
            }
            10 => {
                if p.rts() {
                    11
                } else {
                    12
                }
            }
            11 | 12 => {
                if let Err(e) = p.set_rts(v == 11).await {
                    warn!("RFC 2217 RTS change refused: {e:#}");
                }
                if p.rts() { 11 } else { 12 }
            }
            13..=16 => inbound(p.flow()),
            // 0 (query), 2 (XON/XOFF), DCD / DTR / DSR flow control: the flow in effect.
            _ => r::flow_code(p.flow()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_accept_errors_are_told_from_fatal_ones() {
        use std::io::{Error, ErrorKind};
        for k in [
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::Interrupted,
            ErrorKind::WouldBlock,
        ] {
            assert!(accept_error_is_transient(&Error::from(k)), "{k:?}");
        }
        #[cfg(target_os = "macos")]
        for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ECONNABORTED] {
            assert!(accept_error_is_transient(&Error::from_raw_os_error(code)), "{code}");
        }
        assert!(!accept_error_is_transient(&Error::from(ErrorKind::PermissionDenied)));
        assert!(!accept_error_is_transient(&Error::from(ErrorKind::InvalidInput)));
        #[cfg(target_os = "macos")]
        assert!(!accept_error_is_transient(&Error::from_raw_os_error(libc::EBADF)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_resource_errors_are_transient() {
        use std::io::Error;
        // WSAEMFILE, WSAENOBUFS.
        assert!(accept_error_is_transient(&Error::from_raw_os_error(10024)));
        assert!(accept_error_is_transient(&Error::from_raw_os_error(10055)));
        // WSAEBADF-like errors end the listener.
        assert!(!accept_error_is_transient(&Error::from_raw_os_error(10009)));
    }

    #[test]
    fn bare_port_means_loopback() {
        assert_eq!(parse_listen("7000").unwrap(), "127.0.0.1:7000".parse().unwrap());
        assert_eq!(parse_listen(" 7000 ").unwrap(), "127.0.0.1:7000".parse().unwrap());
        assert_eq!(parse_listen("0.0.0.0:7000").unwrap(), "0.0.0.0:7000".parse().unwrap());
        assert_eq!(parse_listen("[::1]:7000").unwrap(), "[::1]:7000".parse().unwrap());
        assert!(parse_listen("localhost").is_err());
        assert!(parse_listen("70000").is_err());
    }
}
