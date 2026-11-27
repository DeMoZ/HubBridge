//! Fake USB devices for the simulator and tests: real descriptors, standard requests, and
//! these behaviours behind the bulk endpoints:
//! - **loopback**: bytes written to bulk OUT come back on bulk IN;
//! - **serial chips** ([`serial`]): CP210x, FTDI, CH34x and CDC-ACM with their control
//!   requests, decoded settings and test hooks;
//! - **mass storage** (the flash drive fixture): Bulk-Only Transport with a small SCSI
//!   target (INQUIRY, TEST UNIT READY, REQUEST SENSE, READ CAPACITY(10), READ(10),
//!   MODE SENSE(6)); block contents follow [`block_byte`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use hubbridge_proto::{Setup, UsbStatus, descriptors};
use tokio::sync::oneshot;

pub mod serial;

use serial::SerialState;
pub use serial::{LineErrorKind, ModemLines, SerialChip, SerialDecoded, SerialEvent};

use crate::usb::{BoxFut, TransferResult, UsbBackend, UsbDevice, UsbError};

/// Byte `offset` of block `lba` of the fake flash drive.
pub fn block_byte(lba: u32, offset: usize) -> u8 {
    (lba.wrapping_mul(31) as usize).wrapping_add(offset * 7) as u8
}

#[derive(Clone)]
pub enum FakeKind {
    /// Bulk OUT data is echoed on bulk IN.
    Loopback,
    /// Bulk-Only mass storage with `blocks` blocks of 512 bytes.
    MassStorage { blocks: u32 },
    /// A USB serial chip (CP210x, FTDI, CH34x, CDC-ACM) with its control requests.
    Serial(SerialChip),
    /// Only control transfers (descriptors).
    DescriptorsOnly,
    /// Opening panics: tests that a fault inside the agent still answers the host.
    PanicOnOpen,
}

struct FakeEntry {
    raw: Vec<u8>,
    strings: Vec<String>,
    kind: FakeKind,
    permitted: bool,
    /// Serial chip state: survives closing and reopening the device.
    serial: Option<Arc<Mutex<SerialState>>>,
    /// The currently open device, so test hooks can wake its waiting transfers.
    live: Option<Weak<Mutex<State>>>,
}

/// A set of fake devices by agent id.
#[derive(Default)]
pub struct FakeUsb {
    devices: Mutex<HashMap<u32, FakeEntry>>,
}

impl FakeUsb {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeUsb::default())
    }

    /// `strings` are string descriptors 1, 2, 3... (manufacturer, product, serial).
    pub fn add(&self, id: u32, raw: Vec<u8>, strings: &[&str], kind: FakeKind, permitted: bool) {
        let serial = match &kind {
            FakeKind::Serial(chip) => Some(Arc::new(Mutex::new(SerialState::new(*chip, &raw)))),
            _ => None,
        };
        self.devices.lock().unwrap().insert(
            id,
            FakeEntry {
                raw,
                strings: strings.iter().map(|s| s.to_string()).collect(),
                kind,
                permitted,
                serial,
                live: None,
            },
        );
    }

    fn serial_of(&self, id: u32) -> Arc<Mutex<SerialState>> {
        self.devices
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|e| e.serial.clone())
            .unwrap_or_else(|| panic!("fake device {id} is not a serial chip"))
    }

    /// Wake the transfers waiting on the open device after the chip's state changed.
    fn wake(&self, id: u32) {
        let live = self.devices.lock().unwrap().get(&id).and_then(|e| e.live.clone());
        if let Some(state) = live.and_then(|w| w.upgrade()) {
            state.lock().unwrap().pump_all(true);
        }
    }

    /// Every control request and OUT write the serial fake `id` got, in order.
    pub fn serial_log(&self, id: u32) -> Vec<SerialEvent> {
        self.serial_of(id).lock().unwrap().log()
    }

    pub fn clear_serial_log(&self, id: u32) {
        self.serial_of(id).lock().unwrap().clear_log();
    }

    /// What the host programmed into port 0 of serial fake `id`.
    pub fn serial_state(&self, id: u32) -> SerialDecoded {
        self.serial_state_port(id, 0)
    }

    pub fn serial_state_port(&self, id: u32, port: usize) -> SerialDecoded {
        self.serial_of(id).lock().unwrap().decoded(port)
    }

    /// Change the modem lines the device drives; CDC-ACM and CH34x send an interrupt packet,
    /// FTDI reports it in the next status header.
    pub fn set_modem(&self, id: u32, lines: ModemLines) {
        self.serial_of(id).lock().unwrap().set_modem(lines);
        self.wake(id);
    }

    /// Bytes the device sends on its own (not looped back).
    pub fn inject_rx(&self, id: u32, bytes: &[u8]) {
        self.serial_of(id).lock().unwrap().inject_rx(bytes);
        self.wake(id);
    }

    pub fn inject_line_error(&self, id: u32, kind: LineErrorKind) {
        self.serial_of(id).lock().unwrap().inject_line_error(kind);
        self.wake(id);
    }

    /// The next bulk OUT of the serial fake ends with `status` and is not looped back.
    pub fn fail_next_out(&self, id: u32, status: UsbStatus) {
        self.serial_of(id).lock().unwrap().fail_next_out(status);
    }

    /// While set, every bulk IN of the serial fake stalls.
    pub fn set_stall_in(&self, id: u32, on: bool) {
        self.serial_of(id).lock().unwrap().set_stall_in(on);
        self.wake(id);
    }

    /// The next bulk IN ends `Cancelled` but carries `data` (what a cancelled URB can do).
    pub fn cancel_next_in_with(&self, id: u32, data: &[u8]) {
        self.serial_of(id).lock().unwrap().cancel_next_in_with(data.to_vec());
        self.wake(id);
    }

    /// The next bulk IN of the serial fake stalls once.
    pub fn stall_next_in(&self, id: u32) {
        self.serial_of(id).lock().unwrap().stall_next_in();
        self.wake(id);
    }

    pub fn set_permitted(&self, id: u32, permitted: bool) {
        if let Some(e) = self.devices.lock().unwrap().get_mut(&id) {
            e.permitted = permitted;
        }
    }
}

impl UsbBackend for FakeUsb {
    fn open(&self, id: u32) -> BoxFut<Result<Arc<dyn UsbDevice>, UsbError>> {
        let mut devices = self.devices.lock().unwrap();
        let res = match devices.get_mut(&id) {
            None => Err(UsbError::NotFound),
            Some(e) if matches!(e.kind, FakeKind::PanicOnOpen) => {
                return Box::pin(async move { panic!("fake backend: open of device {id} panics") });
            }
            Some(e) if !e.permitted => Err(UsbError::NoPermission("allow USB access on the phone".into())),
            Some(e) => {
                let mut ep_iface = HashMap::new();
                if let Ok(parsed) = descriptors::parse(&e.raw)
                    && let Some(cfg) = parsed.configs.first()
                {
                    for i in &cfg.interfaces {
                        for ep in &i.endpoints {
                            ep_iface.insert(ep.address, i.number);
                        }
                    }
                }
                let state = Arc::new(Mutex::new(State {
                    raw: e.raw.clone(),
                    strings: e.strings.clone(),
                    kind: e.kind.clone(),
                    claimed: HashSet::new(),
                    loop_buf: VecDeque::new(),
                    msc_data: VecDeque::new(),
                    msc_csw: None,
                    msc_stall: false,
                    closed: false,
                    ep_iface,
                    waiters: HashMap::new(),
                    tickers: HashSet::new(),
                    serial: e.serial.clone(),
                }));
                e.live = Some(Arc::downgrade(&state));
                Ok(Arc::new(FakeDevice { state }) as Arc<dyn UsbDevice>)
            }
        };
        Box::pin(async move { res })
    }
}

struct State {
    raw: Vec<u8>,
    strings: Vec<String>,
    kind: FakeKind,
    claimed: HashSet<u8>,
    loop_buf: VecDeque<u8>,
    /// Data-in phase of the current mass-storage command.
    msc_data: VecDeque<u8>,
    msc_csw: Option<[u8; 13]>,
    /// The host expects more data than the command produced: stall bulk IN once (BOT 6.7.2).
    msc_stall: bool,
    closed: bool,
    /// Endpoint address -> number of the interface that owns it.
    ep_iface: HashMap<u8, u8>,
    /// IN transfers waiting for data, per endpoint, in submission order.
    waiters: HashMap<u8, VecDeque<Waiter>>,
    /// Endpoints with a running FTDI idle-status timer.
    tickers: HashSet<u8>,
    serial: Option<Arc<Mutex<SerialState>>>,
}

/// An IN transfer waiting for data. Like the nusb worker, only the front one's deadline
/// counts and when it passes every waiting transfer of the endpoint is cancelled.
struct Waiter {
    length: u32,
    deadline: Instant,
    tx: oneshot::Sender<TransferResult>,
}

pub struct FakeDevice {
    state: Arc<Mutex<State>>,
}

fn ready<T: Send + 'static>(v: T) -> BoxFut<T> {
    Box::pin(async move { v })
}

impl State {
    fn config_slice(&self) -> &[u8] {
        &self.raw[18.min(self.raw.len())..]
    }

    fn control(&mut self, setup: Setup, data: &[u8]) -> TransferResult {
        let len = setup.length as usize;
        match (setup.request_type, setup.request) {
            // GET_DESCRIPTOR
            (0x80, 0x06) => {
                let (kind, index) = ((setup.value >> 8) as u8, (setup.value & 0xff) as u8);
                let bytes: Vec<u8> = match kind {
                    1 => self.raw[..18].to_vec(),
                    2 if index == 0 => self.config_slice().to_vec(),
                    3 if index == 0 => vec![4, 3, 0x09, 0x04],
                    3 => match self.strings.get(index as usize - 1) {
                        Some(s) => {
                            let utf16: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
                            let mut v = vec![(2 + utf16.len()) as u8, 3];
                            v.extend(utf16);
                            v
                        }
                        None => return TransferResult::failed(UsbStatus::Stall),
                    },
                    _ => return TransferResult::failed(UsbStatus::Stall),
                };
                TransferResult::ok_in(bytes.into_iter().take(len).collect())
            }
            // GET_STATUS / GET_CONFIGURATION
            (0x80, 0x00) => TransferResult::ok_in(vec![0, 0].into_iter().take(len).collect()),
            (0x80, 0x08) => TransferResult::ok_in(vec![1].into_iter().take(len).collect()),
            // Mass storage: GET MAX LUN, Bulk-Only reset
            (0xa1, 0xfe) if matches!(self.kind, FakeKind::MassStorage { .. }) => TransferResult::ok_in(vec![0]),
            (0x21, 0xff) if matches!(self.kind, FakeKind::MassStorage { .. }) => {
                self.msc_data.clear();
                self.msc_csw = None;
                TransferResult::ok_out(0)
            }
            _ if self.serial.is_some() => {
                let serial = self.serial.as_ref().unwrap();
                let r = serial.lock().unwrap().control(setup, data);
                r.unwrap_or_else(|| TransferResult::failed(UsbStatus::Stall))
            }
            // Vendor OUT requests of the plain fakes: accept.
            (t, _) if t & 0x80 == 0 && (t >> 5) & 3 == 2 => TransferResult::ok_out(setup.length as usize),
            _ => TransferResult::failed(UsbStatus::Stall),
        }
    }

    fn msc_command(&mut self, cbw: &[u8], blocks: u32) -> bool {
        if cbw.len() != 31 || cbw[..4] != *b"USBC" {
            return false;
        }
        let tag = [cbw[4], cbw[5], cbw[6], cbw[7]];
        let data_len = u32::from_le_bytes([cbw[8], cbw[9], cbw[10], cbw[11]]);
        let cb = &cbw[15..31];
        let mut ok = true;
        let data: Vec<u8> = match cb[0] {
            0x00 => Vec::new(), // TEST UNIT READY
            0x03 => {
                let mut s = vec![0u8; 18];
                s[0] = 0x70;
                s[7] = 10;
                s
            }
            0x12 => {
                let mut v = vec![0x00, 0x80, 0x04, 0x02, 31, 0, 0, 0];
                v.extend_from_slice(b"Generic ");
                v.extend_from_slice(b"Flash Disk      ");
                v.extend_from_slice(b"8.07");
                v.truncate(cb[4] as usize);
                v
            }
            0x1a => vec![3, 0, 0, 0],
            0x25 => {
                let mut v = (blocks - 1).to_be_bytes().to_vec();
                v.extend_from_slice(&512u32.to_be_bytes());
                v
            }
            0x28 => {
                let lba = u32::from_be_bytes([cb[2], cb[3], cb[4], cb[5]]);
                let count = u16::from_be_bytes([cb[7], cb[8]]) as u32;
                if lba.saturating_add(count) > blocks {
                    ok = false;
                    Vec::new()
                } else {
                    let mut v = Vec::with_capacity(count as usize * 512);
                    for b in lba..lba + count {
                        v.extend((0..512).map(|i| block_byte(b, i)));
                    }
                    v
                }
            }
            _ => {
                ok = false;
                Vec::new()
            }
        };
        let data = &data[..data.len().min(data_len as usize)];
        let residue = data_len - data.len() as u32;
        self.msc_data = data.iter().copied().collect();
        self.msc_stall = residue > 0;
        let mut csw = [0u8; 13];
        csw[..4].copy_from_slice(b"USBS");
        csw[4..8].copy_from_slice(&tag);
        csw[8..12].copy_from_slice(&residue.to_le_bytes());
        csw[12] = if ok { 0 } else { 1 };
        self.msc_csw = Some(csw);
        true
    }

    fn endpoint_claimed(&self, endpoint: u8) -> bool {
        self.ep_iface.get(&endpoint).is_some_and(|i| self.claimed.contains(i))
    }

    /// OUT transfer, done at queue time (keeps the host's order).
    fn bulk_out(&mut self, endpoint: u8, data: Vec<u8>) -> TransferResult {
        match self.kind.clone() {
            FakeKind::Loopback => {
                self.loop_buf.extend(&data);
                TransferResult::ok_out(data.len())
            }
            FakeKind::MassStorage { blocks } => {
                if self.msc_command(&data, blocks) {
                    TransferResult::ok_out(data.len())
                } else {
                    TransferResult::failed(UsbStatus::Stall)
                }
            }
            FakeKind::Serial(_) => self.serial.as_ref().unwrap().lock().unwrap().out(endpoint, data),
            FakeKind::DescriptorsOnly | FakeKind::PanicOnOpen => TransferResult::failed(UsbStatus::InvalidArgument),
        }
    }

    /// The result of an IN transfer if the device has something for it now.
    fn bulk_in(&mut self, endpoint: u8, length: u32, idle_tick: bool) -> Option<TransferResult> {
        match self.kind.clone() {
            FakeKind::Loopback => {
                if self.loop_buf.is_empty() {
                    None
                } else {
                    let n = (length as usize).min(self.loop_buf.len());
                    Some(TransferResult::ok_in(self.loop_buf.drain(..n).collect()))
                }
            }
            FakeKind::MassStorage { .. } => {
                if !self.msc_data.is_empty() {
                    let n = (length as usize).min(self.msc_data.len());
                    Some(TransferResult::ok_in(self.msc_data.drain(..n).collect()))
                } else if self.msc_stall {
                    self.msc_stall = false;
                    Some(TransferResult::failed(UsbStatus::Stall))
                } else {
                    self.msc_csw.take().map(|csw| TransferResult::ok_in(csw.to_vec()))
                }
            }
            FakeKind::Serial(_) => self
                .serial
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .in_data(endpoint, length, idle_tick),
            FakeKind::DescriptorsOnly | FakeKind::PanicOnOpen => {
                Some(TransferResult::failed(UsbStatus::InvalidArgument))
            }
        }
    }

    /// Serve waiting IN transfers of `ep` in submission order; when the front one is past its
    /// deadline and got no data, cancel all of them.
    fn pump(&mut self, ep: u8, mut idle_tick: bool) {
        loop {
            let Some(length) = self.waiters.get(&ep).and_then(|q| q.front()).map(|w| w.length) else {
                return;
            };
            // Only the front transfer may take an idle status packet.
            let Some(r) = self.bulk_in(ep, length, idle_tick) else {
                break;
            };
            if let Some(w) = self.waiters.get_mut(&ep).and_then(|q| q.pop_front()) {
                let _ = w.tx.send(r);
            }
            idle_tick = false;
        }
        let expired = self
            .waiters
            .get(&ep)
            .and_then(|q| q.front())
            .is_some_and(|w| w.deadline <= Instant::now());
        if expired && let Some(q) = self.waiters.get_mut(&ep) {
            for w in q.drain(..) {
                let _ = w.tx.send(TransferResult::failed(UsbStatus::Cancelled));
            }
        }
    }

    fn pump_all(&mut self, idle_tick: bool) {
        let eps: Vec<u8> = self.waiters.keys().copied().collect();
        for ep in eps {
            self.pump(ep, idle_tick);
        }
    }
}

impl UsbDevice for FakeDevice {
    fn claim(&self, interface: u8, _detach: bool) -> BoxFut<Result<(), UsbError>> {
        let mut st = self.state.lock().unwrap();
        let res = if st.closed {
            Err(UsbError::Failed("closed".into()))
        } else {
            st.claimed.insert(interface);
            Ok(())
        };
        ready(res)
    }

    fn release(&self, interface: u8) -> BoxFut<Result<(), UsbError>> {
        self.state.lock().unwrap().claimed.remove(&interface);
        ready(Ok(()))
    }

    fn set_alternate(&self, interface: u8, _alternate: u8) -> BoxFut<Result<(), UsbError>> {
        let ok = self.state.lock().unwrap().claimed.contains(&interface);
        ready(if ok {
            Ok(())
        } else {
            Err(UsbError::NotClaimed(format!("interface {interface} is not claimed")))
        })
    }

    fn control(&self, setup: Setup, data: Vec<u8>, _timeout: Duration) -> BoxFut<TransferResult> {
        let res = self.state.lock().unwrap().control(setup, &data);
        ready(res)
    }

    fn transfer(&self, endpoint: u8, length: u32, data: Vec<u8>, timeout: Duration) -> BoxFut<TransferResult> {
        let mut st = self.state.lock().unwrap();
        if st.closed || !st.endpoint_claimed(endpoint) {
            return ready(TransferResult::failed(UsbStatus::InvalidArgument));
        }
        if endpoint & 0x80 == 0 {
            let r = st.bulk_out(endpoint, data);
            // Written data may satisfy IN transfers that are already waiting.
            st.pump_all(false);
            return ready(r);
        }
        let queue_empty = st.waiters.get(&endpoint).is_none_or(|q| q.is_empty());
        if queue_empty && let Some(r) = st.bulk_in(endpoint, length, false) {
            return ready(r);
        }
        // Nothing to read yet: wait for data like a real device would.
        let (tx, rx) = oneshot::channel();
        let deadline = Instant::now() + timeout;
        st.waiters
            .entry(endpoint)
            .or_default()
            .push_back(Waiter { length, deadline, tx });
        // Deadline timer: acts only when this transfer (or an expired one) is at the front.
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            state.lock().unwrap().pump(endpoint, false);
        });
        // FTDI: an idle chip answers every latency-timer period with a status-only packet.
        let period = st.serial.as_ref().and_then(|s| s.lock().unwrap().idle_tick(endpoint));
        if let Some(period) = period
            && st.tickers.insert(endpoint)
        {
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(period).await;
                    let mut st = state.lock().unwrap();
                    if st.closed || st.waiters.get(&endpoint).is_none_or(|q| q.is_empty()) {
                        st.tickers.remove(&endpoint);
                        return;
                    }
                    st.pump(endpoint, true);
                }
            });
        }
        drop(st);
        Box::pin(async move {
            rx.await
                .unwrap_or_else(|_| TransferResult::failed(UsbStatus::Cancelled))
        })
    }

    fn clear_halt(&self, _endpoint: u8) -> BoxFut<Result<(), UsbError>> {
        ready(Ok(()))
    }

    fn reset(&self) -> BoxFut<Result<(), UsbError>> {
        let mut st = self.state.lock().unwrap();
        st.claimed.clear();
        st.loop_buf.clear();
        st.msc_data.clear();
        st.msc_csw = None;
        ready(Ok(()))
    }

    fn close(&self) {
        let mut st = self.state.lock().unwrap();
        st.closed = true;
        st.claimed.clear();
        for (_, q) in st.waiters.drain() {
            for w in q {
                let _ = w.tx.send(TransferResult::failed(UsbStatus::Cancelled));
            }
        }
    }
}

#[cfg(test)]
mod tests;
