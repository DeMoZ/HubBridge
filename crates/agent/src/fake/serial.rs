//! Fake USB serial chips: CP210x, FTDI, CH34x and CDC-ACM.
//!
//! Each chip answers the control requests the Linux drivers (`cp210x.c`, `ftdi_sio.c`,
//! `ch341.c`, `cdc-acm.c`) send, stalls everything else, and loops bytes written to bulk OUT
//! back to bulk IN (CP210x only after IFC_ENABLE, like the chip). Every control request and
//! every OUT write goes into one ordered event log, and the settings the host programmed are
//! decoded into [`SerialDecoded`], so tests can check both what was sent and what it meant.

use std::collections::VecDeque;
use std::time::Duration;

use hubbridge_proto::usb::{InterfaceInfo, TransferType};
use hubbridge_proto::{Setup, UsbStatus, descriptors};

use crate::usb::TransferResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialChip {
    /// `partnum` is what GET_PARTNUM answers (2 = CP2102, 0x20 = CP2102N, 0xff = stalls).
    Cp210x {
        partnum: u8,
    },
    Ftdi,
    /// `version` is what READ_VERSION answers; `limited_prescaler` makes the break register
    /// read stall, the way the quirk is detected.
    Ch34x {
        version: u8,
        limited_prescaler: bool,
    },
    CdcAcm,
}

/// Modem lines the device drives (inputs of the host).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModemLines {
    pub cts: bool,
    pub dsr: bool,
    pub ri: bool,
    pub dcd: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineErrorKind {
    Overrun,
    Parity,
    Framing,
    Break,
}

/// One thing the host did to the fake, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SerialEvent {
    Control { setup: Setup, data: Vec<u8> },
    Out { len: usize },
}

/// What the host programmed into the chip, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialDecoded {
    pub baud: u32,
    pub data_bits: u8,
    /// 0 none, 1 odd, 2 even, 3 mark, 4 space.
    pub parity: u8,
    /// 0 = 1 stop bit, 1 = 1.5, 2 = 2.
    pub stop_bits: u8,
    pub flow_rtscts: bool,
    pub dtr: bool,
    pub rts: bool,
    pub break_on: bool,
    pub purges: u32,
    pub purged_rx: u32,
    pub purged_tx: u32,
    pub enabled: bool,
    pub latency_ms: u8,
}

impl Default for SerialDecoded {
    fn default() -> Self {
        SerialDecoded {
            baud: 9600,
            data_bits: 8,
            parity: 0,
            stop_bits: 0,
            flow_rtscts: false,
            dtr: false,
            rts: false,
            break_on: false,
            purges: 0,
            purged_rx: 0,
            purged_tx: 0,
            enabled: false,
            latency_ms: 16,
        }
    }
}

struct Port {
    /// Interface that owns the control requests (CDC: the control interface).
    iface: u8,
    in_ep: u8,
    out_ep: u8,
    intr_ep: Option<u8>,
    max_packet: u16,
    decoded: SerialDecoded,
    loop_buf: VecDeque<u8>,
    intr_queue: VecDeque<Vec<u8>>,
    /// CDC line coding (dwDTERate LE, format, parity, bits).
    line_coding: [u8; 7],
    /// CP210x flow control block: handshake, flow replace, xon, xoff.
    flow: [u8; 16],
    /// CH34x break register: [break bits, LCR].
    break_reg: [u8; 2],
    /// FTDI line status bits waiting for the next data packet (OE 0x02, PE 0x04, FE 0x08, BI 0x10).
    pending_line: u8,
    /// CP210x ulErrors since the last GET_COMM_STATUS.
    comm_errors: u32,
    /// The next bulk IN stalls once (test hook).
    stall_next_in: bool,
}

pub struct SerialState {
    chip: SerialChip,
    ports: Vec<Port>,
    log: Vec<SerialEvent>,
    modem: ModemLines,
    /// FTDI: chips with bcdDevice 0x0600 and below use channel 0 (no port number).
    ftdi_legacy_channel: bool,
    /// The next bulk OUT ends with this status without reaching the chip (test hook).
    fail_next_out: Option<UsbStatus>,
    /// Every bulk IN stalls while set (test hook).
    stall_in_forever: bool,
    /// The next bulk IN ends `Cancelled` carrying these bytes (test hook).
    cancel_next_in: Option<Vec<u8>>,
}

fn le16(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

fn pick(interfaces: &[InterfaceInfo], want_class: Option<u8>) -> Vec<(u8, u8, u8, u16)> {
    // (interface, in, out, max packet) of every interface that has a bulk pair.
    let mut v = Vec::new();
    for i in interfaces {
        if i.alternate != 0 || want_class.is_some_and(|c| i.class != c) {
            continue;
        }
        let bulk_in = i
            .endpoints
            .iter()
            .find(|e| e.transfer_type() == TransferType::Bulk && e.is_in());
        let bulk_out = i
            .endpoints
            .iter()
            .find(|e| e.transfer_type() == TransferType::Bulk && !e.is_in());
        if let (Some(a), Some(b)) = (bulk_in, bulk_out) {
            v.push((i.number, a.address, b.address, a.max_packet_size));
        }
    }
    v
}

impl SerialState {
    pub fn new(chip: SerialChip, raw: &[u8]) -> Self {
        let parsed = descriptors::parse(raw).expect("fake serial device has valid descriptors");
        let interfaces = parsed.configs.first().map(|c| c.interfaces.clone()).unwrap_or_default();
        let new_port = |iface: u8, in_ep: u8, out_ep: u8, intr_ep: Option<u8>, max_packet: u16| Port {
            iface,
            in_ep,
            out_ep,
            intr_ep,
            max_packet,
            decoded: SerialDecoded::default(),
            loop_buf: VecDeque::new(),
            intr_queue: VecDeque::new(),
            line_coding: [0x80, 0x25, 0, 0, 0, 0, 8],
            flow: {
                // What the CP210x reports before anyone sets flow control.
                let mut f = [0u8; 16];
                f[0..4].copy_from_slice(&1u32.to_le_bytes());
                f[4..8].copy_from_slice(&0x40u32.to_le_bytes());
                f[8..12].copy_from_slice(&128u32.to_le_bytes());
                f[12..16].copy_from_slice(&512u32.to_le_bytes());
                f
            },
            break_reg: [0x01, 0xc3],
            pending_line: 0,
            comm_errors: 0,
            stall_next_in: false,
        };
        let ports = match chip {
            SerialChip::CdcAcm => {
                let ctl = interfaces
                    .iter()
                    .find(|i| i.class == 2 && i.subclass == 2)
                    .expect("CDC control interface");
                let intr = ctl
                    .endpoints
                    .iter()
                    .find(|e| e.transfer_type() == TransferType::Interrupt && e.is_in())
                    .map(|e| e.address);
                let data = pick(&interfaces, Some(0x0a));
                let (_, i, o, mp) = data.first().copied().expect("CDC data interface");
                vec![new_port(ctl.number, i, o, intr, mp)]
            }
            _ => pick(&interfaces, None)
                .into_iter()
                .map(|(n, i, o, mp)| {
                    let intr = interfaces
                        .iter()
                        .find(|x| x.number == n)
                        .and_then(|x| {
                            x.endpoints
                                .iter()
                                .find(|e| e.transfer_type() == TransferType::Interrupt && e.is_in())
                        })
                        .map(|e| e.address);
                    new_port(n, i, o, intr, mp)
                })
                .collect(),
        };
        assert!(!ports.is_empty(), "fake serial device needs a bulk IN/OUT pair");
        SerialState {
            chip,
            ports,
            log: Vec::new(),
            modem: ModemLines::default(),
            ftdi_legacy_channel: parsed.device.bcd_device <= 0x0600 && parsed.device.bcd_device != 0x0500,
            fail_next_out: None,
            stall_in_forever: false,
            cancel_next_in: None,
        }
    }

    pub fn log(&self) -> Vec<SerialEvent> {
        self.log.clone()
    }

    pub fn clear_log(&mut self) {
        self.log.clear();
    }

    pub fn decoded(&self, port: usize) -> SerialDecoded {
        self.ports[port].decoded.clone()
    }

    /// FTDI idle status period of the port that owns `ep`, if it is an FTDI bulk IN endpoint.
    pub fn idle_tick(&self, ep: u8) -> Option<Duration> {
        if self.chip != SerialChip::Ftdi {
            return None;
        }
        self.ports
            .iter()
            .find(|p| p.in_ep == ep)
            .map(|p| Duration::from_millis(p.decoded.latency_ms.max(1) as u64))
    }

    // --- test hooks ---------------------------------------------------------------------

    pub fn inject_rx(&mut self, bytes: &[u8]) {
        self.ports[0].loop_buf.extend(bytes);
    }

    pub fn stall_next_in(&mut self) {
        self.ports[0].stall_next_in = true;
    }

    pub fn fail_next_out(&mut self, status: UsbStatus) {
        self.fail_next_out = Some(status);
    }

    pub fn set_stall_in(&mut self, on: bool) {
        self.stall_in_forever = on;
    }

    pub fn cancel_next_in_with(&mut self, data: Vec<u8>) {
        self.cancel_next_in = Some(data);
    }

    pub fn set_modem(&mut self, lines: ModemLines) {
        if lines == self.modem {
            return;
        }
        self.modem = lines;
        match self.chip {
            SerialChip::CdcAcm => self.queue_cdc_state(0),
            SerialChip::Ch34x { .. } => {
                let bits = self.ch34x_status_bits();
                if let Some(p) = self.ports.first_mut() {
                    p.intr_queue.push_back(vec![0, 0, !bits, 0]);
                }
            }
            _ => {}
        }
    }

    pub fn inject_line_error(&mut self, kind: LineErrorKind) {
        match self.chip {
            SerialChip::CdcAcm => {
                let bit = match kind {
                    LineErrorKind::Break => 1 << 2,
                    LineErrorKind::Framing => 1 << 4,
                    LineErrorKind::Parity => 1 << 5,
                    LineErrorKind::Overrun => 1 << 6,
                };
                self.queue_cdc_state(bit);
            }
            SerialChip::Ftdi => {
                self.ports[0].pending_line |= match kind {
                    LineErrorKind::Overrun => 0x02,
                    LineErrorKind::Parity => 0x04,
                    LineErrorKind::Framing => 0x08,
                    LineErrorKind::Break => 0x10,
                };
            }
            SerialChip::Cp210x { .. } => {
                self.ports[0].comm_errors |= match kind {
                    LineErrorKind::Break => 0x01,
                    LineErrorKind::Framing => 0x02,
                    LineErrorKind::Overrun => 0x04,
                    LineErrorKind::Parity => 0x10,
                };
            }
            SerialChip::Ch34x { .. } => {}
        }
    }

    fn ch34x_status_bits(&self) -> u8 {
        (self.modem.cts as u8) | (self.modem.dsr as u8) << 1 | (self.modem.ri as u8) << 2 | (self.modem.dcd as u8) << 3
    }

    fn queue_cdc_state(&mut self, error_bits: u16) {
        let bitmap: u16 =
            (self.modem.dcd as u16) | (self.modem.dsr as u16) << 1 | (self.modem.ri as u16) << 3 | error_bits;
        let iface = self.ports[0].iface as u16;
        let mut p = vec![0xa1, 0x20, 0, 0];
        p.extend_from_slice(&le16(iface));
        p.extend_from_slice(&le16(2));
        p.extend_from_slice(&le16(bitmap));
        self.ports[0].intr_queue.push_back(p);
    }

    // --- bulk and interrupt -------------------------------------------------------------

    fn port_of_out(&self, ep: u8) -> Option<usize> {
        self.ports.iter().position(|p| p.out_ep == ep)
    }

    pub fn out(&mut self, ep: u8, data: Vec<u8>) -> TransferResult {
        let Some(i) = self.port_of_out(ep) else {
            return TransferResult::failed(UsbStatus::InvalidArgument);
        };
        if let Some(status) = self.fail_next_out.take() {
            return TransferResult::failed(status);
        }
        self.log.push(SerialEvent::Out { len: data.len() });
        let loops = match self.chip {
            SerialChip::Cp210x { .. } => self.ports[i].decoded.enabled,
            _ => true,
        };
        if loops {
            self.ports[i].loop_buf.extend(&data);
        }
        TransferResult::ok_out(data.len())
    }

    fn ftdi_header(&self, line: u8) -> [u8; 2] {
        let m = self.modem;
        let b0 = (m.cts as u8) << 4 | (m.dsr as u8) << 5 | (m.ri as u8) << 6 | (m.dcd as u8) << 7;
        [b0, 0x60 | line]
    }

    /// `None`: nothing to deliver yet (the transfer waits). `idle_tick`: the FTDI latency
    /// timer fired, an empty chip sends a status-only packet.
    pub fn in_data(&mut self, ep: u8, length: u32, idle_tick: bool) -> Option<TransferResult> {
        if let Some(i) = self.ports.iter().position(|p| p.in_ep == ep) {
            if self.stall_in_forever {
                return Some(TransferResult::failed(UsbStatus::Stall));
            }
            if let Some(data) = self.cancel_next_in.take() {
                let actual = data.len() as u32;
                return Some(TransferResult {
                    status: UsbStatus::Cancelled,
                    data,
                    actual,
                });
            }
            if std::mem::take(&mut self.ports[i].stall_next_in) {
                return Some(TransferResult::failed(UsbStatus::Stall));
            }
            if self.chip == SerialChip::Ftdi {
                return self.ftdi_in(i, length, idle_tick);
            }
            let p = &mut self.ports[i];
            if p.loop_buf.is_empty() {
                return None;
            }
            let n = (length as usize).min(p.loop_buf.len());
            return Some(TransferResult::ok_in(p.loop_buf.drain(..n).collect()));
        }
        if let Some(p) = self.ports.iter_mut().find(|p| p.intr_ep == Some(ep)) {
            let pkt = p.intr_queue.pop_front()?;
            let n = pkt.len().min(length as usize);
            if n < pkt.len() {
                // Longer than one packet: the rest comes with the next transfer.
                p.intr_queue.push_front(pkt[n..].to_vec());
            }
            return Some(TransferResult::ok_in(pkt[..n].to_vec()));
        }
        Some(TransferResult::failed(UsbStatus::InvalidArgument))
    }

    fn ftdi_in(&mut self, i: usize, length: u32, idle_tick: bool) -> Option<TransferResult> {
        let mp = self.ports[i].max_packet as usize;
        if self.ports[i].loop_buf.is_empty() {
            return idle_tick.then(|| TransferResult::ok_in(self.ftdi_header(0).to_vec()));
        }
        let mut out = Vec::new();
        let packets = (length as usize / mp).max(1);
        for _ in 0..packets {
            if self.ports[i].loop_buf.is_empty() {
                break;
            }
            let line = std::mem::take(&mut self.ports[i].pending_line);
            out.extend_from_slice(&self.ftdi_header(line));
            let n = (mp - 2).min(self.ports[i].loop_buf.len());
            out.extend(self.ports[i].loop_buf.drain(..n));
        }
        Some(TransferResult::ok_in(out))
    }

    // --- control ------------------------------------------------------------------------

    /// `None`: the chip stalls the request.
    pub fn control(&mut self, setup: Setup, data: &[u8]) -> Option<TransferResult> {
        self.log.push(SerialEvent::Control {
            setup,
            data: data.to_vec(),
        });
        // Standard requests a host may send during setup.
        if matches!((setup.request_type, setup.request), (0x00, 0x09) | (0x01, 0x0b)) {
            return Some(TransferResult::ok_out(0));
        }
        match self.chip {
            SerialChip::Cp210x { partnum } => self.cp210x(partnum, setup, data),
            SerialChip::Ftdi => self.ftdi(setup),
            SerialChip::Ch34x {
                version,
                limited_prescaler,
            } => self.ch34x(version, limited_prescaler, setup),
            SerialChip::CdcAcm => self.cdc(setup, data),
        }
    }

    fn reply(setup: Setup, bytes: &[u8]) -> Option<TransferResult> {
        let n = bytes.len().min(setup.length as usize);
        Some(TransferResult::ok_in(bytes[..n].to_vec()))
    }

    fn ok() -> Option<TransferResult> {
        Some(TransferResult::ok_out(0))
    }

    fn cp210x(&mut self, partnum: u8, s: Setup, data: &[u8]) -> Option<TransferResult> {
        let p = &mut self.ports[0];
        let d = &mut p.decoded;
        match (s.request_type, s.request) {
            (0xc0, 0xff) if s.value == 0x370b => {
                if partnum == 0xff {
                    None
                } else {
                    Self::reply(s, &[partnum])
                }
            }
            (0x41, 0x00) => {
                d.enabled = s.value != 0;
                Self::ok()
            }
            (0x41, 0x1e) if data.len() == 4 => {
                d.baud = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                Self::ok()
            }
            (0x41, 0x03) => {
                d.data_bits = (s.value >> 8) as u8;
                d.parity = ((s.value >> 4) & 0x0f) as u8;
                d.stop_bits = (s.value & 0x0f) as u8;
                Self::ok()
            }
            (0x41, 0x07) => {
                if s.value & 0x100 != 0 {
                    d.dtr = s.value & 1 != 0;
                }
                if s.value & 0x200 != 0 {
                    d.rts = s.value & 2 != 0;
                }
                Self::ok()
            }
            (0x41, 0x05) => {
                d.break_on = s.value != 0;
                Self::ok()
            }
            (0x41, 0x12) => {
                d.purges += 1;
                if s.value & 0x0a != 0 {
                    d.purged_rx += 1;
                    p.loop_buf.clear();
                }
                if s.value & 0x05 != 0 {
                    d.purged_tx += 1;
                }
                Self::ok()
            }
            (0x41, 0x13) if data.len() == 16 => {
                p.flow.copy_from_slice(data);
                let hs = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let repl = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                let rts_mode = (repl >> 6) & 3;
                d.flow_rtscts = hs & 0x08 != 0 && rts_mode == 2;
                d.dtr = hs & 3 == 1;
                d.rts = rts_mode != 0;
                Self::ok()
            }
            (0xc1, 0x14) => Self::reply(s, &p.flow.clone()),
            (0xc1, 0x08) => {
                let m = self.modem;
                let b = d.dtr as u8
                    | (d.rts as u8) << 1
                    | (m.cts as u8) << 4
                    | (m.dsr as u8) << 5
                    | (m.ri as u8) << 6
                    | (m.dcd as u8) << 7;
                Self::reply(s, &[b])
            }
            (0xc1, 0x10) => {
                let mut v = vec![0u8; 19];
                v[0..4].copy_from_slice(&std::mem::take(&mut p.comm_errors).to_le_bytes());
                Self::reply(s, &v)
            }
            _ => None,
        }
    }

    fn ftdi_port(&self, channel: u16) -> Option<usize> {
        if self.ftdi_legacy_channel {
            (channel == 0).then_some(0)
        } else {
            let i = (channel as usize).checked_sub(1)?;
            (i < self.ports.len()).then_some(i)
        }
    }

    fn ftdi_decode_baud(index: u16, value: u16) -> u32 {
        const EIGHTHS: [u32; 8] = [0, 4, 2, 1, 3, 5, 6, 7];
        let hi = (index >> 8) & 0x02 != 0;
        let div = value as u32 | (((index >> 8) as u32 & 0x01) << 16);
        let int = div & 0x3fff;
        let code = ((div >> 14) & 0x07) as usize;
        let divisor3 = match div & 0x1_ffff {
            0 => 8,
            1 => 12,
            _ => int * 8 + EIGHTHS[code],
        };
        if divisor3 == 0 {
            return 0;
        }
        let base = if hi { 96_000_000u64 } else { 24_000_000u64 };
        ((base + divisor3 as u64 / 2) / divisor3 as u64) as u32
    }

    fn ftdi(&mut self, s: Setup) -> Option<TransferResult> {
        // Baud rate carries divisor bits in the high byte of the index.
        let channel = if s.request == 0x03 { s.index & 0xff } else { s.index };
        let i = self.ftdi_port(channel)?;
        let m = self.modem;
        let p = &mut self.ports[i];
        let d = &mut p.decoded;
        match (s.request_type, s.request) {
            (0x40, 0x00) => {
                d.purges += 1;
                if s.value == 0 || s.value == 1 {
                    d.purged_rx += 1;
                    p.loop_buf.clear();
                }
                if s.value == 0 || s.value == 2 {
                    d.purged_tx += 1;
                }
                d.enabled = true;
                Self::ok()
            }
            (0x40, 0x01) => {
                if s.value & 0x100 != 0 {
                    d.dtr = s.value & 1 != 0;
                }
                if s.value & 0x200 != 0 {
                    d.rts = s.value & 2 != 0;
                }
                Self::ok()
            }
            (0x40, 0x02) => {
                d.flow_rtscts = s.index >> 8 == 1;
                Self::ok()
            }
            (0x40, 0x03) => {
                d.baud = Self::ftdi_decode_baud(s.index, s.value);
                Self::ok()
            }
            (0x40, 0x04) => {
                d.data_bits = (s.value & 0xff) as u8;
                d.parity = ((s.value >> 8) & 7) as u8;
                d.stop_bits = ((s.value >> 11) & 7) as u8;
                d.break_on = s.value & 0x4000 != 0;
                Self::ok()
            }
            (0x40, 0x09) => {
                d.latency_ms = s.value as u8;
                Self::ok()
            }
            (0xc0, 0x0a) => Self::reply(s, &[d.latency_ms]),
            (0xc0, 0x05) => {
                let h = self.ftdi_header(0);
                let _ = m;
                Self::reply(s, &h)
            }
            _ => None,
        }
    }

    fn ch34x_decode_baud(index: u16) -> u32 {
        let ps = (index & 0x03) as u32;
        let fact = ((index >> 2) & 1) as u32;
        let div = 0x100 - ((index >> 8) as u32 & 0xff);
        let clk_div = 1u64 << (12 - 3 * ps - fact);
        (48_000_000u64 / (clk_div * div as u64)) as u32
    }

    fn ch34x(&mut self, version: u8, limited: bool, s: Setup) -> Option<TransferResult> {
        let m = self.modem;
        let bits = (m.cts as u8) | (m.dsr as u8) << 1 | (m.ri as u8) << 2 | (m.dcd as u8) << 3;
        let p = &mut self.ports[0];
        let d = &mut p.decoded;
        match (s.request_type, s.request) {
            (0xc0, 0x5f) => Self::reply(s, &[version, 0]),
            (0x40, 0xa1) => {
                d.enabled = true;
                Self::ok()
            }
            (0x40, 0x9a) => match s.value {
                0x1312 => {
                    d.baud = Self::ch34x_decode_baud(s.index);
                    Self::ok()
                }
                0x2518 => {
                    let lcr = s.index as u8;
                    p.break_reg[1] = lcr;
                    d.data_bits = 5 + (lcr & 0x03);
                    d.stop_bits = if lcr & 0x04 != 0 { 2 } else { 0 };
                    d.parity = if lcr & 0x08 == 0 {
                        0
                    } else if lcr & 0x20 != 0 {
                        if lcr & 0x10 != 0 { 4 } else { 3 }
                    } else if lcr & 0x10 != 0 {
                        2
                    } else {
                        1
                    };
                    Self::ok()
                }
                0x1805 => {
                    p.break_reg = [s.index as u8, (s.index >> 8) as u8];
                    d.break_on = p.break_reg[0] & 0x01 == 0;
                    Self::ok()
                }
                _ => None,
            },
            (0xc0, 0x95) => match s.value {
                // ch341_detect_quirks reads register 0x05; a limited chip stalls it.
                0x0005 if !limited => Self::reply(s, &[0x01, 0x00]),
                0x0706 => Self::reply(s, &[!bits, 0]),
                0x1805 if !limited => Self::reply(s, &p.break_reg.clone()),
                _ => None,
            },
            (0x40, 0xa4) => {
                let ctl = !s.value as u8;
                d.dtr = ctl & 0x20 != 0;
                d.rts = ctl & 0x40 != 0;
                Self::ok()
            }
            _ => None,
        }
    }

    fn cdc(&mut self, s: Setup, data: &[u8]) -> Option<TransferResult> {
        let p = &mut self.ports[0];
        if s.index != p.iface as u16 {
            return None;
        }
        let d = &mut p.decoded;
        match (s.request_type, s.request) {
            (0x21, 0x20) if data.len() == 7 => {
                p.line_coding.copy_from_slice(data);
                d.baud = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                d.stop_bits = data[4];
                d.parity = data[5];
                d.data_bits = data[6];
                Self::ok()
            }
            (0xa1, 0x21) => Self::reply(s, &p.line_coding.clone()),
            (0x21, 0x22) => {
                d.dtr = s.value & 1 != 0;
                d.rts = s.value & 2 != 0;
                Self::ok()
            }
            (0x21, 0x23) => {
                d.break_on = s.value != 0;
                Self::ok()
            }
            _ => None,
        }
    }
}
