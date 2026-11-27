//! Silicon Labs CP210x (AN571, Linux `cp210x.c`): vendor requests to the interface.

use super::{Ctl, Flow, Line, LineErrors, ModemStatus, Parity, StopBits};

const OUT: u8 = 0x41; // REQTYPE_HOST_TO_INTERFACE
const IN: u8 = 0xc1; // REQTYPE_INTERFACE_TO_HOST

pub const IFC_ENABLE: u8 = 0x00;
pub const SET_LINE_CTL: u8 = 0x03;
pub const SET_BREAK: u8 = 0x05;
pub const SET_MHS: u8 = 0x07;
pub const GET_MDMSTS: u8 = 0x08;
pub const GET_COMM_STATUS: u8 = 0x10;
pub const PURGE: u8 = 0x12;
pub const SET_FLOW: u8 = 0x13;
pub const GET_FLOW: u8 = 0x14;
pub const SET_BAUDRATE: u8 = 0x1e;
pub const VENDOR_SPECIFIC: u8 = 0xff;
pub const GET_PARTNUM: u16 = 0x370b;

pub const PARTNUM_UNKNOWN: u8 = 0xff;

pub fn ifc_enable(interface: u8, on: bool) -> Ctl {
    Ctl::out(OUT, IFC_ENABLE, on as u16, interface as u16, Vec::new())
}

pub fn set_baudrate(interface: u8, baud: u32) -> Ctl {
    Ctl::out(OUT, SET_BAUDRATE, 0, interface as u16, baud.to_le_bytes().to_vec())
}

/// SET_LINE_CTL value: stop bits in bits 0-3, parity 4-7, data bits 8-15.
pub fn line_ctl_value(line: &Line) -> u16 {
    line.stop_bits.code() as u16 | (line.parity.code() as u16) << 4 | (line.data_bits as u16) << 8
}

pub fn set_line_ctl(interface: u8, line: &Line) -> Ctl {
    Ctl::out(OUT, SET_LINE_CTL, line_ctl_value(line), interface as u16, Vec::new())
}

/// SET_MHS: a line is written only when given.
pub fn set_mhs(interface: u8, dtr: Option<bool>, rts: Option<bool>) -> Ctl {
    let mut v = 0u16;
    if let Some(on) = dtr {
        v |= 0x0100 | on as u16;
    }
    if let Some(on) = rts {
        v |= 0x0200 | (on as u16) << 1;
    }
    Ctl::out(OUT, SET_MHS, v, interface as u16, Vec::new())
}

pub fn get_flow(interface: u8) -> Ctl {
    Ctl::read(IN, GET_FLOW, 0, interface as u16, 16)
}

const CTS_HANDSHAKE: u32 = 1 << 3;
const DTR_MASK: u32 = 3;
const DTR_ACTIVE: u32 = 1;
const RTS_SHIFT: u32 = 6;
const RTS_MASK: u32 = 3 << RTS_SHIFT;

/// The 16-byte SET_FLOW block (`cp210x_set_flow_control`): `current` is what GET_FLOW
/// returned (limits and unrelated bits are kept). With RTS/CTS the CTS handshake is on and
/// RTS is in flow-control mode (2) when requested, else inactive (0); without it RTS is
/// plainly active (1) or inactive (0).
pub fn flow_block(current: &[u8], flow: Flow, dtr: bool, rts: bool) -> Vec<u8> {
    let mut b = [0u8; 16];
    let n = current.len().min(16);
    b[..n].copy_from_slice(&current[..n]);
    let mut hs = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let mut repl = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    // DSR / DCD handshakes and sensitivity are not used.
    hs &= !((1 << 4) | (1 << 5) | (1 << 6));
    hs &= !DTR_MASK;
    if dtr {
        hs |= DTR_ACTIVE;
    }
    repl &= !RTS_MASK;
    match (flow, rts) {
        (Flow::RtsCts, rts) => {
            hs |= CTS_HANDSHAKE;
            if rts {
                repl |= 2 << RTS_SHIFT;
            }
        }
        (Flow::None, rts) => {
            hs &= !CTS_HANDSHAKE;
            if rts {
                repl |= 1 << RTS_SHIFT;
            }
        }
    }
    b[0..4].copy_from_slice(&hs.to_le_bytes());
    b[4..8].copy_from_slice(&repl.to_le_bytes());
    b.to_vec()
}

pub fn set_flow(interface: u8, block: Vec<u8>) -> Ctl {
    Ctl::out(OUT, SET_FLOW, 0, interface as u16, block)
}

pub fn set_break(interface: u8, on: bool) -> Ctl {
    Ctl::out(OUT, SET_BREAK, on as u16, interface as u16, Vec::new())
}

/// PURGE bits: 0 or 2 clear the transmit queue, 1 or 3 the receive queue.
pub fn purge(interface: u8, rx: bool, tx: bool) -> Ctl {
    let mut v = 0u16;
    if tx {
        v |= 0x0005;
    }
    if rx {
        v |= 0x000a;
    }
    Ctl::out(OUT, PURGE, v, interface as u16, Vec::new())
}

pub fn get_mdmsts(interface: u8) -> Ctl {
    Ctl::read(IN, GET_MDMSTS, 0, interface as u16, 1)
}

pub fn parse_mdmsts(b: u8) -> ModemStatus {
    ModemStatus {
        cts: b & 0x10 != 0,
        dsr: b & 0x20 != 0,
        ri: b & 0x40 != 0,
        dcd: b & 0x80 != 0,
    }
}

pub fn get_comm_status(interface: u8) -> Ctl {
    Ctl::read(IN, GET_COMM_STATUS, 0, interface as u16, 19)
}

/// ulErrors of GET_COMM_STATUS (cleared by reading): break, framing, hardware or queue
/// overrun, parity.
pub fn parse_comm_status(data: &[u8]) -> LineErrors {
    if data.len() < 4 {
        return LineErrors::default();
    }
    let e = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    LineErrors {
        brk: e & 0x01 != 0,
        framing: e & 0x02 != 0,
        overrun: e & 0x0c != 0,
        parity: e & 0x10 != 0,
    }
}

pub fn get_partnum(interface: u8) -> Ctl {
    Ctl::read(0xc0, VENDOR_SPECIFIC, GET_PARTNUM, interface as u16, 1)
}

pub fn part_name(partnum: u8) -> &'static str {
    match partnum {
        0x01 => "CP2101",
        0x02 => "CP2102",
        0x03 => "CP2103",
        0x04 => "CP2104",
        0x05 => "CP2105",
        0x08 => "CP2108",
        0x20..=0x22 => "CP2102N",
        _ => "CP210x",
    }
}

/// Highest baud rate of a part (`cp210x_init_max_speed`).
pub fn max_baud(partnum: u8) -> u32 {
    match partnum {
        0x01 => 921_600,
        0x02 | 0x03 => 1_000_000,
        0x20..=0x22 => 3_000_000,
        _ => 2_000_000,
    }
}

pub const MIN_BAUD: u32 = 300;

/// Refuse a rate the part cannot do, naming its maximum.
pub fn check_baud(partnum: u8, baud: u32) -> Result<(), String> {
    let max = max_baud(partnum);
    if baud > max {
        return Err(format!(
            "{} supports at most {max} baud, not {baud}",
            part_name(partnum)
        ));
    }
    if baud < MIN_BAUD {
        return Err(format!(
            "{} supports at least {MIN_BAUD} baud, not {baud}",
            part_name(partnum)
        ));
    }
    Ok(())
}

/// The settings a part can really do: the CP2101 only has 8 data bits, 1 stop bit and no
/// mark/space parity.
pub fn effective_line(partnum: u8, mut line: Line) -> Line {
    if partnum == 0x01 {
        line.data_bits = 8;
        line.stop_bits = StopBits::One;
        if matches!(line.parity, Parity::Mark | Parity::Space) {
            line.parity = Parity::None;
        }
    }
    if !(5..=8).contains(&line.data_bits) {
        line.data_bits = 8;
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_sequence_vectors() {
        let c = ifc_enable(0, true);
        assert_eq!(
            (c.setup.request_type, c.setup.request, c.setup.value, c.setup.index),
            (0x41, 0x00, 1, 0)
        );
        let c = set_baudrate(0, 115_200);
        assert_eq!((c.setup.request, c.setup.length), (0x1e, 4));
        assert_eq!(c.data, vec![0x00, 0xc2, 0x01, 0x00]);
        assert_eq!(set_line_ctl(0, &Line::default()).setup.value, 0x0800);
        assert_eq!(ifc_enable(0, false).setup.value, 0);
    }

    #[test]
    fn seven_even_two_is_0x0722() {
        let line = Line {
            baud: 9600,
            data_bits: 7,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
        };
        assert_eq!(line_ctl_value(&line), 0x0722);
        let mark = Line {
            parity: Parity::Mark,
            stop_bits: StopBits::OnePointFive,
            ..line
        };
        assert_eq!(line_ctl_value(&mark), 0x0731);
    }

    #[test]
    fn mhs_masks() {
        assert_eq!(set_mhs(0, Some(true), Some(true)).setup.value, 0x0303);
        assert_eq!(set_mhs(0, Some(false), Some(false)).setup.value, 0x0300);
        assert_eq!(set_mhs(0, Some(true), None).setup.value, 0x0101);
        assert_eq!(set_mhs(0, None, Some(false)).setup.value, 0x0200);
    }

    #[test]
    fn flow_block_rts_cts() {
        let mut current = [0u8; 16];
        current[0..4].copy_from_slice(&1u32.to_le_bytes());
        current[4..8].copy_from_slice(&0x40u32.to_le_bytes());
        current[8..12].copy_from_slice(&128u32.to_le_bytes());
        current[12..16].copy_from_slice(&512u32.to_le_bytes());
        let b = flow_block(&current, Flow::RtsCts, true, true);
        assert_eq!(b.len(), 16);
        let hs = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let repl = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        assert_ne!(hs & 0x08, 0, "CTS handshake");
        assert_eq!(hs & 3, 1, "DTR active");
        assert_eq!((repl >> 6) & 3, 2, "RTS flow control");
        assert_eq!(&b[8..], &current[8..], "limits kept");
        // Off again, RTS plain active, DTR off.
        let b = flow_block(&b, Flow::None, false, true);
        let hs = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let repl = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        assert_eq!((hs & 0x08, hs & 3), (0, 0));
        assert_eq!((repl >> 6) & 3, 1);
        // Both off with RTS/CTS: handshake on, RTS inactive.
        let b = flow_block(&current, Flow::RtsCts, false, false);
        assert_eq!((u32::from_le_bytes([b[4], b[5], b[6], b[7]]) >> 6) & 3, 0);
    }

    #[test]
    fn purge_and_status_parsers() {
        assert_eq!(purge(0, true, true).setup.value, 0x000f);
        assert_eq!(purge(0, true, false).setup.value, 0x000a);
        assert_eq!(purge(0, false, true).setup.value, 0x0005);
        let m = parse_mdmsts(0x93);
        assert!(m.cts && m.dcd && !m.dsr && !m.ri);
        let mut st = vec![0u8; 19];
        st[0] = 0x04;
        let e = parse_comm_status(&st);
        assert!(e.overrun && !e.framing);
        st[0] = 0x13;
        let e = parse_comm_status(&st);
        assert!(e.brk && e.framing && e.parity && !e.overrun);
        assert!(!parse_comm_status(&[]).any());
    }

    #[test]
    fn part_limits() {
        assert_eq!(max_baud(0x02), 1_000_000);
        let e = check_baud(0x02, 3_000_000).unwrap_err();
        assert!(e.contains("1000000") && e.contains("CP2102"), "{e}");
        assert!(check_baud(0x20, 3_000_000).is_ok());
        assert_eq!(max_baud(0x01), 921_600);
        assert_eq!(max_baud(PARTNUM_UNKNOWN), 2_000_000);
        assert!(check_baud(0x02, 100).is_err());
    }

    #[test]
    fn cp2101_only_does_8n1() {
        let line = Line {
            baud: 9600,
            data_bits: 7,
            parity: Parity::Mark,
            stop_bits: StopBits::Two,
        };
        let e = effective_line(0x01, line);
        assert_eq!((e.data_bits, e.parity, e.stop_bits), (8, Parity::None, StopBits::One));
        assert_eq!(effective_line(0x02, line), line);
    }
}
