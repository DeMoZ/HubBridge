//! The serial port: opening a device on the phone as a UART.
//!
//! One [`SerialPort`] owns the open device and runs, as tasks on the tokio runtime:
//! - the **IN pump**: `in_flight` bulk IN transfers always queued, replies awaited in order
//!   (data is delivered whatever the status: a `Cancelled` reply may carry bytes);
//! - the **OUT writer**: queued bytes go out in up to 4 transfers of up to 16 KiB;
//! - the **modem status source**: the interrupt endpoint of CDC-ACM and CH34x, the in-band
//!   header of FTDI, `GET_MDMSTS` polling for CP210x while someone wants it.
//!
//! The agent runs control transfers concurrently, so every control sequence of the port is
//! serialised by one lock (`ctl_lock`); line changes first wait for the bytes already
//! written to leave (like `TCSADRAIN`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hubbridge_proto::{ErrorCode, UsbDeviceInfo, UsbStatus};
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;

use super::detect::{Detected, DriverKind, detect};
use super::ftdi::FtdiType;
use super::{Ctl, Flow, Line, LineErrors, ModemStatus, PortEvent, SerialConfig, Stats, cdc_acm, ch34x, cp210x, ftdi};
use crate::Client;
use crate::client::{AgentError, PendingTransfer, UsbReply};

/// Bytes buffered for the front end before more received data is dropped.
pub const RX_LIMIT: usize = 1 << 20;
/// Bytes queued for the device before `write` waits.
pub const TX_LIMIT: usize = 64 * 1024;
/// Largest OUT transfer.
pub const OUT_TRANSFER: usize = 16 * 1024;
/// OUT transfers in flight.
pub const OUT_IN_FLIGHT: usize = 4;

#[derive(Debug, Clone)]
pub struct PortOptions {
    /// Which port of a multi-port device (FTDI interface, dual CDC); 0 = the first.
    pub port: usize,
    /// IN transfers kept in flight.
    pub in_flight: usize,
    /// Size of one IN transfer, rounded down to the endpoint's max packet size.
    pub in_transfer: u32,
    /// IN timeout; the agent caps it at 60 s.
    pub in_timeout: Duration,
    pub ftdi_latency_ms: u8,
    pub control_timeout: Duration,
    /// CP210x modem status polling period.
    pub modem_poll: Duration,
}

impl Default for PortOptions {
    fn default() -> Self {
        PortOptions {
            port: 0,
            in_flight: 4,
            in_transfer: 16 * 1024,
            in_timeout: Duration::from_secs(60),
            ftdi_latency_ms: 16,
            control_timeout: Duration::from_secs(2),
            modem_poll: Duration::from_millis(200),
        }
    }
}

/// What the front ends and `hubbridge serial` show about an open port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    pub driver: &'static str,
    pub chip: Option<String>,
    /// `FTDI (FT232R)`.
    pub label: String,
    pub line: Line,
    pub flow: Flow,
}

// --- pure reply handling -------------------------------------------------------------------

/// What the IN pump does after a reply (the data is delivered first, whatever the status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InAction {
    Resubmit,
    ClearHalt,
    Stop(String),
}

/// Decide from a reply status; `errors_in_row` counts failures without a good reply between.
pub fn classify_in(status: UsbStatus, errors_in_row: &mut u32) -> InAction {
    match status {
        UsbStatus::Ok | UsbStatus::Cancelled => {
            *errors_in_row = 0;
            InAction::Resubmit
        }
        UsbStatus::Stall => {
            *errors_in_row = 0;
            InAction::ClearHalt
        }
        UsbStatus::Disconnected => InAction::Stop("device disconnected from the phone".into()),
        UsbStatus::InvalidArgument | UsbStatus::Fault | UsbStatus::Unknown => {
            *errors_in_row += 1;
            if *errors_in_row >= 3 {
                InAction::Stop(format!("IN transfers keep failing ({status:?})"))
            } else {
                InAction::Resubmit
            }
        }
    }
}

// --- receive buffer ------------------------------------------------------------------------

/// Bytes from the device waiting for the front end, at most [`RX_LIMIT`].
pub struct RxBuffer {
    inner: Mutex<RxInner>,
    notify: Notify,
}

struct RxInner {
    q: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

impl RxBuffer {
    fn new() -> Self {
        RxBuffer {
            inner: Mutex::new(RxInner {
                q: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    /// Queue `data`; returns how many bytes did not fit and were dropped.
    fn push(&self, mut data: Vec<u8>) -> usize {
        let mut g = self.inner.lock().unwrap();
        let room = RX_LIMIT.saturating_sub(g.bytes);
        let dropped = data.len().saturating_sub(room);
        data.truncate(room);
        if !data.is_empty() {
            g.bytes += data.len();
            g.q.push_back(data);
            self.notify.notify_one();
        }
        dropped
    }

    /// The next chunk, or `None` once the port is closed and everything was read.
    pub async fn recv(&self) -> Option<Vec<u8>> {
        loop {
            {
                let mut g = self.inner.lock().unwrap();
                if let Some(c) = g.q.pop_front() {
                    g.bytes -= c.len();
                    return Some(c);
                }
                if g.closed {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }

    /// Everything queued right now (empty when nothing is waiting).
    pub fn try_take(&self) -> Vec<u8> {
        let mut g = self.inner.lock().unwrap();
        let mut out = Vec::with_capacity(g.bytes);
        while let Some(c) = g.q.pop_front() {
            out.extend(c);
        }
        g.bytes = 0;
        out
    }

    /// Discard what is buffered (purge of the receive direction).
    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap();
        g.q.clear();
        g.bytes = 0;
    }

    pub fn buffered(&self) -> usize {
        self.inner.lock().unwrap().bytes
    }

    fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.notify.notify_waiters();
        self.notify.notify_one();
    }
}

// --- transmit queue ------------------------------------------------------------------------

#[derive(Default)]
struct TxState {
    q: VecDeque<u8>,
    in_flight: usize,
    in_flight_bytes: usize,
    /// Bytes ever accepted / finished (sent, failed or purged): `drain` compares them.
    enqueued: u64,
    completed: u64,
    closed: bool,
}

struct Tx {
    st: Mutex<TxState>,
    changed: Notify,
}

/// Wait until `cond` holds; `n` is notified (`notify_waiters`) whenever it may have changed.
async fn wait_until(n: &Notify, mut cond: impl FnMut() -> bool) {
    loop {
        let f = n.notified();
        tokio::pin!(f);
        f.as_mut().enable();
        if cond() {
            return;
        }
        f.await;
    }
}

async fn wait_done(rx: &mut watch::Receiver<Option<String>>) {
    let _ = rx.wait_for(|v| v.is_some()).await;
}

// --- the port ------------------------------------------------------------------------------

#[derive(Default)]
struct AtomicStats {
    tx: AtomicU64,
    rx: AtomicU64,
    rx_dropped: AtomicU64,
    overruns: AtomicU64,
    parity: AtomicU64,
    framing: AtomicU64,
    breaks: AtomicU64,
}

struct ChipState {
    line: Line,
    flow: Flow,
    dtr: bool,
    rts: bool,
    brk: bool,
    /// CP210x part number (0xff unknown).
    partnum: u8,
    ftdi: Option<FtdiType>,
    ch_version: u8,
    ch_limited: bool,
    /// Chip name when known (part number, FTDI type).
    chip: Option<String>,
}

struct Inner {
    client: Arc<Client>,
    id: u32,
    det: Detected,
    opts: PortOptions,
    ctl_lock: tokio::sync::Mutex<()>,
    chip: Mutex<ChipState>,
    stats: AtomicStats,
    rx: Arc<RxBuffer>,
    tx: Tx,
    modem: Mutex<ModemStatus>,
    demand: AtomicUsize,
    events: broadcast::Sender<PortEvent>,
    done_tx: watch::Sender<Option<String>>,
    device_lost: AtomicBool,
    closed: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// CDC notifications longer than the interrupt endpoint's packet arrive in pieces.
    notifications: Mutex<Vec<u8>>,
}

#[derive(Clone)]
pub struct SerialPort {
    inner: Arc<Inner>,
}

/// A control request answered with a STALL (tolerated by some callers).
#[derive(Debug, thiserror::Error)]
#[error("request stalled")]
pub struct Stalled;

impl Inner {
    fn name(&self) -> &'static str {
        self.det.kind.name()
    }

    fn chip_state(&self) -> std::sync::MutexGuard<'_, ChipState> {
        self.chip.lock().unwrap()
    }

    /// Send one control request; transport failures are returned, the status is not judged.
    async fn raw_reply(&self, what: &str, c: Ctl) -> Result<UsbReply> {
        match self
            .client
            .control(self.id, c.setup, c.data, self.opts.control_timeout)
            .await
        {
            Ok(r) => Ok(r),
            Err(e) => {
                self.note_error(&e);
                Err(e.context(format!("{} {what}", self.name())))
            }
        }
    }

    /// Send one control request and require success; returns the data read.
    async fn raw(&self, what: &str, c: Ctl) -> Result<Vec<u8>> {
        let r = self.raw_reply(what, c).await?;
        match r.status {
            UsbStatus::Ok => Ok(r.data),
            UsbStatus::Stall => {
                Err(anyhow!(Stalled).context(format!("{} {what} was refused by the device", self.name())))
            }
            s => bail!("{} {what} failed: {s:?}", self.name()),
        }
    }

    /// A transport error that means the device or the session is gone stops the port.
    fn note_error(&self, e: &anyhow::Error) {
        if let Some(reason) = fatal_reason(e) {
            self.device_lost.store(true, Ordering::Relaxed);
            self.stop_with(reason);
        }
    }

    fn stop_with(&self, reason: String) {
        self.done_tx.send_if_modified(|v| {
            if v.is_none() {
                *v = Some(reason);
                true
            } else {
                false
            }
        });
        self.rx.close();
        self.tx.st.lock().unwrap().closed = true;
        self.tx.changed.notify_waiters();
    }

    fn is_done(&self) -> bool {
        self.done_tx.borrow().is_some()
    }

    fn set_modem(&self, m: ModemStatus) {
        let mut cur = self.modem.lock().unwrap();
        if *cur != m {
            *cur = m;
            let _ = self.events.send(PortEvent::Modem(m));
        }
    }

    fn line_errors(&self, e: LineErrors) {
        if !e.any() {
            return;
        }
        let bump = |c: &AtomicU64, on: bool| {
            if on {
                c.fetch_add(1, Ordering::Relaxed);
            }
        };
        bump(&self.stats.overruns, e.overrun);
        bump(&self.stats.parity, e.parity);
        bump(&self.stats.framing, e.framing);
        bump(&self.stats.breaks, e.brk);
        let _ = self.events.send(PortEvent::LineError(e));
    }

    fn deliver(&self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        let n = data.len();
        let dropped = self.rx.push(data);
        self.stats.rx.fetch_add((n - dropped) as u64, Ordering::Relaxed);
        if dropped > 0 {
            self.stats.rx_dropped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }
}

/// A transport error that ends the port, with the reason to show.
fn fatal_reason(e: &anyhow::Error) -> Option<String> {
    if let Some(a) = e.downcast_ref::<AgentError>() {
        return match a.code {
            ErrorCode::NotOpen | ErrorCode::UnknownDevice => Some("device closed on the phone (unplugged?)".into()),
            _ => None,
        };
    }
    let text = format!("{e:#}");
    if text.contains("connection lost") {
        Some("connection to the phone lost".into())
    } else if text.contains("request timed out") {
        Some("the phone stopped answering".into())
    } else {
        None
    }
}

impl SerialPort {
    /// Open `info` as a serial port: detect, open, claim (detaching the phone's driver),
    /// run the chip's initialisation and apply `cfg`. The device is closed again on failure.
    pub async fn open(
        client: Arc<Client>,
        info: &UsbDeviceInfo,
        cfg: SerialConfig,
        opts: PortOptions,
    ) -> Result<SerialPort> {
        // Refused before anything is opened.
        let mut det = detect(info, opts.port).map_err(|e| anyhow!(e))?;
        let id = info.id;
        client.open_device(id).await?;
        match Self::open_opened(&client, info, &mut det, cfg, opts).await {
            Ok(p) => Ok(p),
            Err((claimed, e)) => {
                for i in claimed.iter().rev() {
                    let _ = client.release_interface(id, *i).await;
                }
                let _ = client.close_device(id).await;
                Err(e)
            }
        }
    }

    async fn open_opened(
        client: &Arc<Client>,
        info: &UsbDeviceInfo,
        det: &mut Detected,
        cfg: SerialConfig,
        opts: PortOptions,
    ) -> std::result::Result<SerialPort, (Vec<u8>, anyhow::Error)> {
        let id = info.id;
        let mut claimed: Vec<u8> = Vec::new();
        let res: Result<Arc<Inner>> = async {
            // Without raw descriptors (no permission when listed) read them live and look again:
            // CDC pairing and capabilities need them.
            if info.raw_descriptors.is_none() {
                let (raw, _) = crate::msc::read_descriptors(client, id)
                    .await
                    .context("read the device descriptors")?;
                let mut live = info.clone();
                live.raw_descriptors = Some(raw);
                live.apply_raw_descriptors()
                    .map_err(|e| anyhow!("descriptors from the device: {e}"))?;
                *det = detect(&live, opts.port).map_err(|e| anyhow!(e))?;
            }
            for i in det.claim.clone() {
                client
                    .claim_interface(id, i, true)
                    .await
                    .with_context(|| format!("claim interface {i}"))?;
                claimed.push(i);
            }
            if det.data_alternate != 0 {
                client
                    .set_alternate(id, det.data_interface, det.data_alternate)
                    .await
                    .context("select the data interface setting")?;
            }
            let (events, _) = broadcast::channel(64);
            let (done_tx, _) = watch::channel(None);
            let inner = Arc::new(Inner {
                client: Arc::clone(client),
                id,
                det: det.clone(),
                opts: opts.clone(),
                ctl_lock: tokio::sync::Mutex::new(()),
                chip: Mutex::new(ChipState {
                    line: cfg.line,
                    flow: Flow::None,
                    dtr: false,
                    rts: false,
                    brk: false,
                    partnum: cp210x::PARTNUM_UNKNOWN,
                    ftdi: None,
                    ch_version: 0,
                    ch_limited: false,
                    chip: det.chip.clone(),
                }),
                stats: AtomicStats::default(),
                rx: Arc::new(RxBuffer::new()),
                tx: Tx {
                    st: Mutex::new(TxState::default()),
                    changed: Notify::new(),
                },
                modem: Mutex::new(ModemStatus::default()),
                demand: AtomicUsize::new(0),
                events,
                done_tx,
                device_lost: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                tasks: Mutex::new(Vec::new()),
                notifications: Mutex::new(Vec::new()),
            });
            {
                let _g = inner.ctl_lock.lock().await;
                configure(&inner, &cfg).await?;
            }
            Ok(inner)
        }
        .await;
        let inner = res.map_err(|e| (claimed.clone(), e))?;
        spawn_tasks(&inner);
        Ok(SerialPort { inner })
    }

    // --- information ------------------------------------------------------------------

    pub fn device_id(&self) -> u32 {
        self.inner.id
    }

    pub fn detected(&self) -> &Detected {
        &self.inner.det
    }

    pub fn info(&self) -> PortInfo {
        let c = self.inner.chip_state();
        let driver = self.inner.name();
        let label = match &c.chip {
            Some(chip) => format!("{driver} ({chip})"),
            None => driver.to_string(),
        };
        PortInfo {
            driver,
            chip: c.chip.clone(),
            label,
            line: c.line,
            flow: c.flow,
        }
    }

    pub fn line(&self) -> Line {
        self.inner.chip_state().line
    }

    pub fn flow(&self) -> Flow {
        self.inner.chip_state().flow
    }

    pub fn dtr(&self) -> bool {
        self.inner.chip_state().dtr
    }

    pub fn rts(&self) -> bool {
        self.inner.chip_state().rts
    }

    pub fn break_on(&self) -> bool {
        self.inner.chip_state().brk
    }

    pub fn stats(&self) -> Stats {
        let s = &self.inner.stats;
        Stats {
            tx: s.tx.load(Ordering::Relaxed),
            rx: s.rx.load(Ordering::Relaxed),
            rx_dropped: s.rx_dropped.load(Ordering::Relaxed),
            overruns: s.overruns.load(Ordering::Relaxed),
            parity: s.parity.load(Ordering::Relaxed),
            framing: s.framing.load(Ordering::Relaxed),
            breaks: s.breaks.load(Ordering::Relaxed),
        }
    }

    /// Last modem status the chip reported.
    pub fn modem_status(&self) -> ModemStatus {
        *self.inner.modem.lock().unwrap()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PortEvent> {
        self.inner.events.subscribe()
    }

    /// The bytes received from the device, for one consumer.
    pub fn rx(&self) -> Arc<RxBuffer> {
        Arc::clone(&self.inner.rx)
    }

    /// A consumer wants modem status events: CP210x is polled while any consumer does.
    pub fn want_modem_status(&self, on: bool) {
        if on {
            self.inner.demand.fetch_add(1, Ordering::Relaxed);
        } else {
            let d = &self.inner.demand;
            let mut cur = d.load(Ordering::Relaxed);
            while cur > 0 {
                match d.compare_exchange(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(v) => cur = v,
                }
            }
        }
    }

    /// Resolves with the reason once the port has stopped.
    pub async fn done(&self) -> String {
        let mut rx = self.inner.done_tx.subscribe();
        match rx.wait_for(|v| v.is_some()).await {
            Ok(v) => v.clone().unwrap_or_default(),
            Err(_) => "port dropped".into(),
        }
    }

    pub fn is_done(&self) -> Option<String> {
        self.inner.done_tx.borrow().clone()
    }

    // --- data -------------------------------------------------------------------------

    /// Queue bytes for the device; waits while 64 KiB are still on their way.
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let tx = &self.inner.tx;
        wait_until(&tx.changed, || {
            let g = tx.st.lock().unwrap();
            g.closed || g.q.len() + g.in_flight_bytes < TX_LIMIT
        })
        .await;
        {
            let mut g = tx.st.lock().unwrap();
            if g.closed {
                bail!("the serial port is closed");
            }
            g.q.extend(data);
            g.enqueued += data.len() as u64;
        }
        tx.changed.notify_waiters();
        Ok(())
    }

    /// Wait until everything written so far has been sent.
    pub async fn drain(&self) {
        let tx = &self.inner.tx;
        let target = tx.st.lock().unwrap().enqueued;
        wait_until(&tx.changed, || {
            let g = tx.st.lock().unwrap();
            g.closed || g.completed >= target
        })
        .await;
    }

    // --- line and lines ---------------------------------------------------------------

    /// Change the line format (after the bytes written so far went out). Returns the
    /// settings in effect, which may differ where the chip cannot do what was asked; a rate
    /// the chip cannot do is an error and the line keeps its previous settings.
    pub async fn set_line(&self, line: Line) -> Result<Line> {
        self.drain().await;
        let i = &self.inner;
        let _g = i.ctl_lock.lock().await;
        set_line_locked(i, line).await
    }

    pub async fn set_flow(&self, flow: Flow) -> Result<Flow> {
        self.drain().await;
        let i = &self.inner;
        let _g = i.ctl_lock.lock().await;
        set_flow_locked(i, flow).await
    }

    pub async fn set_dtr(&self, on: bool) -> Result<()> {
        self.set_lines(Some(on), None).await
    }

    pub async fn set_rts(&self, on: bool) -> Result<()> {
        self.set_lines(None, Some(on)).await
    }

    /// Set DTR and/or RTS (only the given ones).
    pub async fn set_lines(&self, dtr: Option<bool>, rts: Option<bool>) -> Result<()> {
        let i = &self.inner;
        let _g = i.ctl_lock.lock().await;
        set_lines_locked(i, dtr, rts).await
    }

    pub async fn set_break(&self, on: bool) -> Result<()> {
        let i = &self.inner;
        let _g = i.ctl_lock.lock().await;
        set_break_locked(i, on).await
    }

    /// Discard buffered data in the chosen directions: the host's own buffers and, where the
    /// chip has a request for it (CP210x, FTDI), the chip's.
    pub async fn purge(&self, rx: bool, tx: bool) -> Result<()> {
        let i = &self.inner;
        if rx {
            i.rx.clear();
        }
        if tx {
            let mut g = i.tx.st.lock().unwrap();
            let n = g.q.len() as u64;
            g.q.clear();
            g.completed += n;
            drop(g);
            i.tx.changed.notify_waiters();
        }
        let _g = i.ctl_lock.lock().await;
        match i.det.kind {
            DriverKind::Cp210x => {
                i.raw("PURGE", cp210x::purge(i.det.control_interface, rx, tx)).await?;
            }
            DriverKind::Ftdi => {
                let ty = i.chip_state().ftdi.expect("FTDI type known after open");
                if rx {
                    i.raw("purge RX", ftdi::reset(ty, i.det.control_interface, 1)).await?;
                }
                if tx {
                    i.raw("purge TX", ftdi::reset(ty, i.det.control_interface, 2)).await?;
                }
            }
            DriverKind::Ch34x | DriverKind::CdcAcm => {}
        }
        Ok(())
    }

    /// Read the chip's error counters into [`Stats`] (CP210x only: others report errors in-band).
    pub async fn refresh_errors(&self) -> Result<()> {
        let i = &self.inner;
        if i.det.kind != DriverKind::Cp210x {
            return Ok(());
        }
        let _g = i.ctl_lock.lock().await;
        let d = i
            .raw("GET_COMM_STATUS", cp210x::get_comm_status(i.det.control_interface))
            .await?;
        i.line_errors(cp210x::parse_comm_status(&d));
        Ok(())
    }

    // --- end --------------------------------------------------------------------------

    /// Drop DTR and RTS, disable the chip's interface where it has such a request, release
    /// the interfaces and close the device. Safe to call more than once and after the port
    /// stopped by itself.
    pub async fn close(&self) {
        let i = &self.inner;
        if i.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        i.stop_with("port closed".into());
        let tasks: Vec<_> = std::mem::take(&mut *i.tasks.lock().unwrap());
        for t in tasks {
            t.abort();
        }
        if !i.device_lost.load(Ordering::Relaxed) {
            let _g = i.ctl_lock.lock().await;
            let _ = shutdown_chip(i).await;
            for n in i.det.claim.iter().rev() {
                let _ = i.client.release_interface(i.id, *n).await;
            }
        }
        let _ = i.client.close_device(i.id).await;
    }
}

// --- chip operations (the control lock is held) -------------------------------------------------

async fn configure(i: &Inner, cfg: &SerialConfig) -> Result<()> {
    let iface = i.det.control_interface;
    match i.det.kind {
        DriverKind::Cp210x => {
            let partnum = match i.raw_reply("GET_PARTNUM", cp210x::get_partnum(iface)).await? {
                r if r.status == UsbStatus::Ok && !r.data.is_empty() => r.data[0],
                _ => cp210x::PARTNUM_UNKNOWN,
            };
            cp210x::check_baud(partnum, cfg.line.baud).map_err(|e| anyhow!(e))?;
            let line = cp210x::effective_line(partnum, cfg.line);
            i.raw("IFC_ENABLE", cp210x::ifc_enable(iface, true)).await?;
            i.raw("SET_BAUDRATE", cp210x::set_baudrate(iface, line.baud)).await?;
            i.raw("SET_LINE_CTL", cp210x::set_line_ctl(iface, &line)).await?;
            let cur = i.raw("GET_FLOW", cp210x::get_flow(iface)).await?;
            let block = cp210x::flow_block(&cur, cfg.flow, cfg.dtr, cfg.rts);
            i.raw("SET_FLOW", cp210x::set_flow(iface, block)).await?;
            // With RTS/CTS the chip drives RTS itself: only DTR is written.
            let rts = (cfg.flow == Flow::None).then_some(cfg.rts);
            i.raw("SET_MHS", cp210x::set_mhs(iface, Some(cfg.dtr), rts)).await?;
            let mut c = i.chip_state();
            c.partnum = partnum;
            c.chip = Some(cp210x::part_name(partnum).to_string()).filter(|n| n != "CP210x");
            c.line = line;
            c.flow = cfg.flow;
            c.dtr = cfg.dtr;
            c.rts = cfg.rts;
        }
        DriverKind::Ftdi => {
            let ty = FtdiType::from_bcd(i.det.bcd_device)
                .ok_or_else(|| anyhow!("unsupported FTDI chip (bcdDevice 0x{:04x})", i.det.bcd_device))?;
            let line = ftdi::effective_line(cfg.line);
            // Validate the rate before the chip is touched.
            let baud = ftdi::set_baud(ty, iface, line.baud).map_err(|e| anyhow!(e))?;
            i.raw("RESET", ftdi::reset(ty, iface, 0)).await?;
            i.raw(
                "SET_LATENCY_TIMER",
                ftdi::set_latency_timer(ty, iface, i.opts.ftdi_latency_ms),
            )
            .await?;
            i.raw("SET_BAUD_RATE", baud).await?;
            i.raw("SET_DATA", ftdi::set_data(ty, iface, &line, false)).await?;
            i.raw("SET_FLOW_CTRL", ftdi::set_flow_ctrl(ty, iface, cfg.flow)).await?;
            for c in ftdi::modem_ctrl(ty, iface, Some(cfg.dtr), Some(cfg.rts)) {
                i.raw("MODEM_CTRL", c).await?;
            }
            let mut c = i.chip_state();
            c.ftdi = Some(ty);
            c.chip = Some(ty.name().to_string());
            c.line = line;
            c.flow = cfg.flow;
            c.dtr = cfg.dtr;
            c.rts = cfg.rts;
        }
        DriverKind::Ch34x => {
            let version = i.raw("READ_VERSION", ch34x::read_version()).await?;
            let version = *version.first().ok_or_else(|| anyhow!("CH34x gave no chip version"))?;
            // The chip variants without break support (and with a limited prescaler) stall
            // the read of the break register.
            let limited = match i.raw_reply("read break register", ch34x::read_break_reg()).await? {
                r if r.status == UsbStatus::Ok => false,
                r if r.status == UsbStatus::Stall => true,
                r => bail!("CH34x read break register failed: {:?}", r.status),
            };
            let line = ch34x::effective_line(cfg.line);
            let baud = ch34x::set_baud(version, line.baud, limited).map_err(|e| anyhow!(e))?;
            i.raw("SERIAL_INIT", ch34x::serial_init()).await?;
            i.raw("WRITE_REG baud", baud).await?;
            if let Some(lcr) = ch34x::set_lcr(version, &line) {
                i.raw("WRITE_REG LCR", lcr).await?;
            }
            i.raw("MODEM_CTRL", ch34x::modem_ctrl(cfg.dtr, cfg.rts)).await?;
            if let Ok(d) = i.raw("READ_REG modem status", ch34x::read_modem_status()).await
                && let Some(m) = ch34x::parse_status_reg(&d)
            {
                i.set_modem(m);
            }
            let mut c = i.chip_state();
            c.ch_version = version;
            c.ch_limited = limited;
            c.chip = Some(format!("CH34x v0x{version:02x}"));
            c.line = line;
            c.flow = Flow::None;
            c.dtr = cfg.dtr;
            c.rts = cfg.rts;
        }
        DriverKind::CdcAcm => {
            // Native CDC devices often ignore the line coding: a STALL is not an error.
            let r = i
                .raw_reply("SET_LINE_CODING", cdc_acm::set_line_coding(iface, &cfg.line))
                .await?;
            if !matches!(r.status, UsbStatus::Ok | UsbStatus::Stall) {
                bail!("CDC-ACM SET_LINE_CODING failed: {:?}", r.status);
            }
            i.raw(
                "SET_CONTROL_LINE_STATE",
                cdc_acm::control_line_state(iface, cfg.dtr, cfg.rts),
            )
            .await?;
            let mut c = i.chip_state();
            c.line = cfg.line;
            c.flow = Flow::None;
            c.dtr = cfg.dtr;
            c.rts = cfg.rts;
        }
    }
    Ok(())
}

async fn set_line_locked(i: &Inner, line: Line) -> Result<Line> {
    let iface = i.det.control_interface;
    let cur = i.chip_state().line;
    match i.det.kind {
        DriverKind::Cp210x => {
            let partnum = i.chip_state().partnum;
            cp210x::check_baud(partnum, line.baud).map_err(|e| anyhow!(e))?;
            let eff = cp210x::effective_line(partnum, line);
            if eff.baud != cur.baud {
                i.raw("SET_BAUDRATE", cp210x::set_baudrate(iface, eff.baud)).await?;
            }
            if (eff.data_bits, eff.parity, eff.stop_bits) != (cur.data_bits, cur.parity, cur.stop_bits) {
                i.raw("SET_LINE_CTL", cp210x::set_line_ctl(iface, &eff)).await?;
            }
            i.chip_state().line = eff;
            Ok(eff)
        }
        DriverKind::Ftdi => {
            let (ty, brk) = {
                let c = i.chip_state();
                (c.ftdi.expect("FTDI type known after open"), c.brk)
            };
            let eff = ftdi::effective_line(line);
            let baud = ftdi::set_baud(ty, iface, eff.baud).map_err(|e| anyhow!(e))?;
            if (eff.data_bits, eff.parity, eff.stop_bits) != (cur.data_bits, cur.parity, cur.stop_bits) {
                i.raw("SET_DATA", ftdi::set_data(ty, iface, &eff, brk)).await?;
            }
            if eff.baud != cur.baud {
                i.raw("SET_BAUD_RATE", baud).await?;
            }
            i.chip_state().line = eff;
            Ok(eff)
        }
        DriverKind::Ch34x => {
            let (version, limited) = {
                let c = i.chip_state();
                (c.ch_version, c.ch_limited)
            };
            let eff = ch34x::effective_line(line);
            let baud = ch34x::set_baud(version, eff.baud, limited).map_err(|e| anyhow!(e))?;
            if eff != cur {
                i.raw("WRITE_REG baud", baud).await?;
                if let Some(lcr) = ch34x::set_lcr(version, &eff) {
                    i.raw("WRITE_REG LCR", lcr).await?;
                }
            }
            i.chip_state().line = eff;
            Ok(eff)
        }
        DriverKind::CdcAcm => {
            if line != cur {
                let r = i
                    .raw_reply("SET_LINE_CODING", cdc_acm::set_line_coding(iface, &line))
                    .await?;
                if !matches!(r.status, UsbStatus::Ok | UsbStatus::Stall) {
                    bail!("CDC-ACM SET_LINE_CODING failed: {:?}", r.status);
                }
            }
            i.chip_state().line = line;
            Ok(line)
        }
    }
}

async fn set_flow_locked(i: &Inner, flow: Flow) -> Result<Flow> {
    let iface = i.det.control_interface;
    let (dtr, rts) = {
        let c = i.chip_state();
        (c.dtr, c.rts)
    };
    let eff = match i.det.kind {
        DriverKind::Cp210x => {
            let cur = i.raw("GET_FLOW", cp210x::get_flow(iface)).await?;
            let block = cp210x::flow_block(&cur, flow, dtr, rts);
            i.raw("SET_FLOW", cp210x::set_flow(iface, block)).await?;
            flow
        }
        DriverKind::Ftdi => {
            let ty = i.chip_state().ftdi.expect("FTDI type known after open");
            i.raw("SET_FLOW_CTRL", ftdi::set_flow_ctrl(ty, iface, flow)).await?;
            flow
        }
        // No hardware flow control in these drivers: report what is in effect.
        DriverKind::Ch34x | DriverKind::CdcAcm => Flow::None,
    };
    i.chip_state().flow = eff;
    Ok(eff)
}

async fn set_lines_locked(i: &Inner, dtr: Option<bool>, rts: Option<bool>) -> Result<()> {
    let iface = i.det.control_interface;
    let (cur_dtr, cur_rts, flow) = {
        let c = i.chip_state();
        (c.dtr, c.rts, c.flow)
    };
    let (new_dtr, new_rts) = (dtr.unwrap_or(cur_dtr), rts.unwrap_or(cur_rts));
    match i.det.kind {
        DriverKind::Cp210x => {
            if flow == Flow::RtsCts && rts.is_some() {
                // With RTS/CTS the chip's RTS is set through the flow block.
                let cur = i.raw("GET_FLOW", cp210x::get_flow(iface)).await?;
                let block = cp210x::flow_block(&cur, flow, new_dtr, new_rts);
                i.raw("SET_FLOW", cp210x::set_flow(iface, block)).await?;
            } else {
                i.raw("SET_MHS", cp210x::set_mhs(iface, dtr, rts)).await?;
            }
        }
        DriverKind::Ftdi => {
            let ty = i.chip_state().ftdi.expect("FTDI type known after open");
            for c in ftdi::modem_ctrl(ty, iface, dtr, rts) {
                i.raw("MODEM_CTRL", c).await?;
            }
        }
        DriverKind::Ch34x => {
            i.raw("MODEM_CTRL", ch34x::modem_ctrl(new_dtr, new_rts)).await?;
        }
        DriverKind::CdcAcm => {
            i.raw(
                "SET_CONTROL_LINE_STATE",
                cdc_acm::control_line_state(iface, new_dtr, new_rts),
            )
            .await?;
        }
    }
    let mut c = i.chip_state();
    c.dtr = new_dtr;
    c.rts = new_rts;
    Ok(())
}

async fn set_break_locked(i: &Inner, on: bool) -> Result<()> {
    let iface = i.det.control_interface;
    match i.det.kind {
        DriverKind::Cp210x => {
            i.raw("SET_BREAK", cp210x::set_break(iface, on)).await?;
        }
        DriverKind::Ftdi => {
            let (ty, line) = {
                let c = i.chip_state();
                (c.ftdi.expect("FTDI type known after open"), c.line)
            };
            i.raw("SET_DATA (break)", ftdi::set_data(ty, iface, &line, on)).await?;
        }
        DriverKind::Ch34x => {
            if i.chip_state().ch_limited {
                bail!("this CH34x chip does not support break");
            }
            let regs = i.raw("read break register", ch34x::read_break_reg()).await?;
            if regs.len() < 2 {
                bail!("CH34x break register: short reply");
            }
            i.raw("WRITE_REG break", ch34x::break_write([regs[0], regs[1]], on))
                .await?;
        }
        DriverKind::CdcAcm => {
            if i.det.acm_capabilities & cdc_acm::CAP_SEND_BREAK == 0 {
                bail!("this CDC-ACM device does not announce break support");
            }
            i.raw("SEND_BREAK", cdc_acm::send_break(iface, on)).await?;
        }
    }
    i.chip_state().brk = on;
    Ok(())
}

/// Close-time requests: lines off, CP210x purge and interface disable. Failures are ignored.
async fn shutdown_chip(i: &Inner) -> Result<()> {
    let iface = i.det.control_interface;
    let _ = set_lines_locked(i, Some(false), Some(false)).await;
    if i.det.kind == DriverKind::Cp210x {
        let _ = i.raw("PURGE", cp210x::purge(iface, true, true)).await;
        i.raw("IFC_ENABLE", cp210x::ifc_enable(iface, false)).await?;
    }
    Ok(())
}

// --- tasks ------------------------------------------------------------------------------------

fn spawn_tasks(inner: &Arc<Inner>) {
    let mut tasks = inner.tasks.lock().unwrap();
    tasks.push(tokio::spawn(in_pump(Arc::clone(inner), Pump::Bulk)));
    tasks.push(tokio::spawn(out_writer(Arc::clone(inner))));
    if inner.det.interrupt_in.is_some() && matches!(inner.det.kind, DriverKind::CdcAcm | DriverKind::Ch34x) {
        tasks.push(tokio::spawn(in_pump(Arc::clone(inner), Pump::Interrupt)));
    }
    if inner.det.kind == DriverKind::Cp210x {
        tasks.push(tokio::spawn(cp210x_poller(Arc::clone(inner))));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pump {
    Bulk,
    Interrupt,
}

/// Keep IN transfers queued on the bulk or the interrupt endpoint and handle the replies.
async fn in_pump(inner: Arc<Inner>, kind: Pump) {
    let (ep, depth, len) = match kind {
        Pump::Bulk => {
            let mp = inner.det.max_packet_in.max(1) as u32;
            let len = (inner.opts.in_transfer / mp).max(1) * mp;
            (inner.det.bulk_in, inner.opts.in_flight.max(1), len)
        }
        Pump::Interrupt => {
            let (ep, mp) = inner.det.interrupt_in.expect("interrupt endpoint");
            (ep, 1, mp.max(1) as u32)
        }
    };
    let mut fifo: VecDeque<PendingTransfer> = VecDeque::new();
    let mut errors_in_row = 0u32;
    let mut done = inner.done_tx.subscribe();
    loop {
        while fifo.len() < depth {
            match inner
                .client
                .submit_transfer(inner.id, ep, len, Vec::new(), inner.opts.in_timeout)
                .await
            {
                Ok(p) => fifo.push_back(p),
                Err(e) => {
                    inner.note_error(&e);
                    inner.stop_with(format!("IN transfer could not be sent: {e:#}"));
                    return;
                }
            }
        }
        let Some(p) = fifo.pop_front() else { return };
        let reply = tokio::select! {
            r = p.wait() => r,
            _ = wait_done(&mut done) => return,
        };
        let r = match reply {
            Ok(r) => r,
            Err(e) => {
                inner.note_error(&e);
                inner.stop_with(format!("IN transfer failed: {e:#}"));
                return;
            }
        };
        // The bytes that arrived count whatever the status.
        match kind {
            Pump::Bulk => handle_bulk_data(&inner, r.data),
            Pump::Interrupt => handle_interrupt_data(&inner, &r.data),
        }
        match classify_in(r.status, &mut errors_in_row) {
            InAction::Resubmit => {}
            InAction::ClearHalt => {
                if let Err(e) = inner.client.clear_halt(inner.id, ep).await {
                    inner.note_error(&e);
                }
            }
            InAction::Stop(reason) => {
                // A failing status endpoint does not end the port; the data path does.
                if kind == Pump::Bulk {
                    inner.stop_with(reason);
                }
                return;
            }
        }
    }
}

fn handle_bulk_data(inner: &Inner, data: Vec<u8>) {
    if inner.det.kind != DriverKind::Ftdi {
        inner.deliver(data);
        return;
    }
    let s = ftdi::split_in(&data, inner.det.max_packet_in as usize);
    if let Some(m) = s.modem {
        inner.set_modem(m);
    }
    inner.line_errors(s.errors);
    inner.deliver(s.data);
}

fn handle_interrupt_data(inner: &Inner, data: &[u8]) {
    match inner.det.kind {
        DriverKind::CdcAcm => {
            let mut buf = inner.notifications.lock().unwrap();
            buf.extend_from_slice(data);
            // A notification is an 8-byte header and `wLength` bytes: take whole ones.
            let mut done = 0;
            while buf.len() - done >= 8 {
                let total = 8 + u16::from_le_bytes([buf[done + 6], buf[done + 7]]) as usize;
                if buf.len() - done < total {
                    break;
                }
                for (m, e) in cdc_acm::parse_notifications(&buf[done..done + total]) {
                    inner.set_modem(m);
                    inner.line_errors(e);
                }
                done += total;
            }
            buf.drain(..done);
        }
        DriverKind::Ch34x => {
            if let Some(m) = ch34x::parse_interrupt(data) {
                inner.set_modem(m);
            }
        }
        _ => {}
    }
}

/// Send queued bytes: up to 4 transfers of up to 16 KiB in flight, whatever is queued when a
/// slot frees up goes out together.
async fn out_writer(inner: Arc<Inner>) {
    let tx = &inner.tx;
    loop {
        wait_until(&tx.changed, || {
            let g = tx.st.lock().unwrap();
            g.closed || (!g.q.is_empty() && g.in_flight < OUT_IN_FLIGHT)
        })
        .await;
        let chunk: Vec<u8> = {
            let mut g = tx.st.lock().unwrap();
            if g.closed {
                return;
            }
            let n = g.q.len().min(OUT_TRANSFER);
            let c: Vec<u8> = g.q.drain(..n).collect();
            g.in_flight += 1;
            g.in_flight_bytes += n;
            c
        };
        let n = chunk.len();
        let sent = inner
            .client
            .submit_transfer(inner.id, inner.det.bulk_out, 0, chunk, Duration::from_secs(10))
            .await;
        match sent {
            Ok(p) => {
                let inner = Arc::clone(&inner);
                tokio::spawn(async move {
                    let r = p.wait().await;
                    finish_out(&inner, n, r).await;
                });
            }
            Err(e) => {
                inner.note_error(&e);
                inner.stop_with(format!("OUT transfer could not be sent: {e:#}"));
                return;
            }
        }
    }
}

async fn finish_out(inner: &Inner, n: usize, r: Result<UsbReply>) {
    let mut stop = None;
    match &r {
        Ok(r) if r.status == UsbStatus::Ok => {
            inner
                .stats
                .tx
                .fetch_add(r.actual.min(n as u32) as u64, Ordering::Relaxed);
        }
        Ok(r) if r.status == UsbStatus::Stall => {
            let _ = inner.client.clear_halt(inner.id, inner.det.bulk_out).await;
        }
        Ok(r) if r.status == UsbStatus::Disconnected => stop = Some("device disconnected from the phone".to_string()),
        Ok(_) => {}
        Err(e) => {
            inner.note_error(e);
            stop = Some(format!("OUT transfer failed: {e:#}"));
        }
    }
    {
        let mut g = inner.tx.st.lock().unwrap();
        g.in_flight -= 1;
        g.in_flight_bytes -= n;
        g.completed += n as u64;
    }
    inner.tx.changed.notify_waiters();
    if let Some(reason) = stop {
        inner.stop_with(reason);
    }
}

/// CP210x: read GET_MDMSTS every `modem_poll` while some consumer wants modem status.
async fn cp210x_poller(inner: Arc<Inner>) {
    let mut done = inner.done_tx.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(inner.opts.modem_poll) => {}
            _ = wait_done(&mut done) => return,
        }
        if inner.demand.load(Ordering::Relaxed) == 0 {
            continue;
        }
        let _g = inner.ctl_lock.lock().await;
        if inner.is_done() {
            return;
        }
        match inner
            .raw("GET_MDMSTS", cp210x::get_mdmsts(inner.det.control_interface))
            .await
        {
            Ok(d) if !d.is_empty() => inner.set_modem(cp210x::parse_mdmsts(d[0])),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_and_cancelled_resubmit() {
        let mut n = 2;
        assert_eq!(classify_in(UsbStatus::Ok, &mut n), InAction::Resubmit);
        assert_eq!(n, 0);
        n = 2;
        assert_eq!(classify_in(UsbStatus::Cancelled, &mut n), InAction::Resubmit);
        assert_eq!(n, 0);
    }

    #[test]
    fn stall_clears_the_halt() {
        let mut n = 1;
        assert_eq!(classify_in(UsbStatus::Stall, &mut n), InAction::ClearHalt);
        assert_eq!(n, 0);
    }

    #[test]
    fn hard_errors_stop_after_three_in_a_row() {
        for s in [UsbStatus::InvalidArgument, UsbStatus::Fault, UsbStatus::Unknown] {
            let mut n = 0;
            assert_eq!(classify_in(s, &mut n), InAction::Resubmit);
            assert_eq!(classify_in(s, &mut n), InAction::Resubmit);
            assert!(matches!(classify_in(s, &mut n), InAction::Stop(_)), "{s:?}");
        }
        // A good reply in between resets the count.
        let mut n = 0;
        classify_in(UsbStatus::Fault, &mut n);
        classify_in(UsbStatus::Fault, &mut n);
        classify_in(UsbStatus::Ok, &mut n);
        assert_eq!(classify_in(UsbStatus::Fault, &mut n), InAction::Resubmit);
    }

    #[test]
    fn disconnect_stops_at_once() {
        let mut n = 0;
        match classify_in(UsbStatus::Disconnected, &mut n) {
            InAction::Stop(r) => assert!(r.contains("disconnected")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fatal_errors_are_recognised() {
        let e: anyhow::Error = AgentError {
            code: ErrorCode::NotOpen,
            message: "x".into(),
        }
        .into();
        assert!(fatal_reason(&e).unwrap().contains("device closed"));
        let e: anyhow::Error = AgentError {
            code: ErrorCode::UsbFailed,
            message: "x".into(),
        }
        .into();
        assert!(fatal_reason(&e).is_none());
        assert!(fatal_reason(&anyhow!("connection lost")).is_some());
        assert!(fatal_reason(&anyhow!("request timed out")).is_some());
        assert!(fatal_reason(&anyhow!("something else")).is_none());
    }

    #[tokio::test]
    async fn rx_buffer_orders_drops_and_clears() {
        let rx = RxBuffer::new();
        assert_eq!(rx.push(vec![1, 2]), 0);
        assert_eq!(rx.push(vec![3]), 0);
        assert_eq!(rx.recv().await, Some(vec![1, 2]));
        assert_eq!(rx.buffered(), 1);
        // Beyond the limit the excess is dropped and reported.
        assert_eq!(rx.push(vec![0; RX_LIMIT]), 1);
        assert_eq!(rx.buffered(), RX_LIMIT);
        rx.clear();
        assert_eq!(rx.buffered(), 0);
        rx.push(vec![9]);
        rx.close();
        assert_eq!(rx.recv().await, Some(vec![9]));
        assert_eq!(rx.recv().await, None);
    }
}
