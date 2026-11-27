//! Fake USB devices for the simulator and tests: real descriptors, standard requests, and
//! two behaviours behind the bulk endpoints:
//! - **loopback** (the CP2102 fixture): bytes written to bulk OUT come back on bulk IN;
//! - **mass storage** (the flash drive fixture): Bulk-Only Transport with a small SCSI
//!   target (INQUIRY, TEST UNIT READY, REQUEST SENSE, READ CAPACITY(10), READ(10),
//!   MODE SENSE(6)); block contents follow [`block_byte`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hubbridge_proto::{Setup, UsbStatus};

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
        self.devices.lock().unwrap().insert(
            id,
            FakeEntry {
                raw,
                strings: strings.iter().map(|s| s.to_string()).collect(),
                kind,
                permitted,
            },
        );
    }

    pub fn set_permitted(&self, id: u32, permitted: bool) {
        if let Some(e) = self.devices.lock().unwrap().get_mut(&id) {
            e.permitted = permitted;
        }
    }
}

impl UsbBackend for FakeUsb {
    fn open(&self, id: u32) -> BoxFut<Result<Arc<dyn UsbDevice>, UsbError>> {
        let res = match self.devices.lock().unwrap().get(&id) {
            None => Err(UsbError::NotFound),
            Some(e) if matches!(e.kind, FakeKind::PanicOnOpen) => {
                return Box::pin(async move { panic!("fake backend: open of device {id} panics") });
            }
            Some(e) if !e.permitted => Err(UsbError::NoPermission("allow USB access on the phone".into())),
            Some(e) => Ok(Arc::new(FakeDevice {
                state: Arc::new(Mutex::new(State {
                    raw: e.raw.clone(),
                    strings: e.strings.clone(),
                    kind: e.kind.clone(),
                    claimed: HashSet::new(),
                    loop_buf: VecDeque::new(),
                    msc_data: VecDeque::new(),
                    msc_csw: None,
                    msc_stall: false,
                    closed: false,
                })),
            }) as Arc<dyn UsbDevice>),
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

    fn control(&mut self, setup: Setup, _data: &[u8]) -> TransferResult {
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
            // Vendor OUT requests (e.g. CP210x line settings): accept.
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

    /// Bulk transfer done at queue time (keeps the host's order).
    fn bulk(&mut self, endpoint: u8, length: u32, data: Vec<u8>) -> Option<TransferResult> {
        if !self.claimed.contains(&0) {
            return Some(TransferResult::failed(UsbStatus::InvalidArgument));
        }
        let is_in = endpoint & 0x80 != 0;
        match (self.kind.clone(), is_in) {
            (FakeKind::Loopback, false) => {
                self.loop_buf.extend(&data);
                Some(TransferResult::ok_out(data.len()))
            }
            (FakeKind::Loopback, true) => {
                if self.loop_buf.is_empty() {
                    None // wait for data
                } else {
                    let n = (length as usize).min(self.loop_buf.len());
                    Some(TransferResult::ok_in(self.loop_buf.drain(..n).collect()))
                }
            }
            (FakeKind::MassStorage { blocks }, false) => {
                if self.msc_command(&data, blocks) {
                    Some(TransferResult::ok_out(data.len()))
                } else {
                    Some(TransferResult::failed(UsbStatus::Stall))
                }
            }
            (FakeKind::MassStorage { .. }, true) => {
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
            (FakeKind::DescriptorsOnly | FakeKind::PanicOnOpen, _) => {
                Some(TransferResult::failed(UsbStatus::InvalidArgument))
            }
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
        if let Some(r) = self.state.lock().unwrap().bulk(endpoint, length, data) {
            return ready(r);
        }
        // An IN with nothing to read yet: wait for data like a real device would.
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let deadline = Instant::now() + timeout;
            loop {
                tokio::time::sleep(Duration::from_millis(2)).await;
                if let Some(r) = state.lock().unwrap().bulk(endpoint, length, Vec::new()) {
                    return r;
                }
                if Instant::now() >= deadline {
                    return TransferResult::failed(UsbStatus::Cancelled);
                }
            }
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
    }
}
