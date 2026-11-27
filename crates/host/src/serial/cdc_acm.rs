//! CDC-ACM (USB CDC PSTN 1.2, Linux `cdc-acm.c`): class requests to the control interface.

use super::{Ctl, Line, LineErrors, ModemStatus};

pub const SET_LINE_CODING: u8 = 0x20;
pub const GET_LINE_CODING: u8 = 0x21;
pub const SET_CONTROL_LINE_STATE: u8 = 0x22;
pub const SEND_BREAK: u8 = 0x23;
/// ACM `bmCapabilities`: the device supports SET/GET_LINE_CODING.
pub const CAP_LINE_CODING: u8 = 0x02;
/// ACM `bmCapabilities`: the device supports SEND_BREAK.
pub const CAP_SEND_BREAK: u8 = 0x04;

/// The 7-byte line coding: dwDTERate (LE), bCharFormat, bParityType, bDataBits.
pub fn line_coding_data(line: &Line) -> Vec<u8> {
    let mut v = line.baud.to_le_bytes().to_vec();
    v.push(line.stop_bits.code());
    v.push(line.parity.code());
    v.push(line.data_bits);
    v
}

pub fn set_line_coding(interface: u8, line: &Line) -> Ctl {
    Ctl::out(0x21, SET_LINE_CODING, 0, interface as u16, line_coding_data(line))
}

pub fn control_line_state(interface: u8, dtr: bool, rts: bool) -> Ctl {
    Ctl::out(
        0x21,
        SET_CONTROL_LINE_STATE,
        dtr as u16 | (rts as u16) << 1,
        interface as u16,
        Vec::new(),
    )
}

pub fn send_break(interface: u8, on: bool) -> Ctl {
    Ctl::out(
        0x21,
        SEND_BREAK,
        if on { 0xffff } else { 0 },
        interface as u16,
        Vec::new(),
    )
}

/// SERIAL_STATE notifications in an interrupt transfer (there may be several): modem lines
/// (DCD, DSR, RI; CDC has no CTS) and the line errors reported with them.
pub fn parse_notifications(data: &[u8]) -> Vec<(ModemStatus, LineErrors)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let kind = data[pos + 1];
        let len = u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as usize;
        if pos + 8 + len > data.len() {
            break;
        }
        if data[pos] == 0xa1 && kind == 0x20 && len >= 2 {
            let b = u16::from_le_bytes([data[pos + 8], data[pos + 9]]);
            out.push((
                ModemStatus {
                    cts: false,
                    dsr: b & 0x02 != 0,
                    ri: b & 0x08 != 0,
                    dcd: b & 0x01 != 0,
                },
                LineErrors {
                    brk: b & 0x04 != 0,
                    framing: b & 0x10 != 0,
                    parity: b & 0x20 != 0,
                    overrun: b & 0x40 != 0,
                },
            ));
        }
        pos += 8 + len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::{Parity, StopBits};

    #[test]
    fn line_coding_115200_8n1() {
        let c = set_line_coding(0, &Line::default());
        assert_eq!(c.setup.request_type, 0x21);
        assert_eq!(c.setup.request, 0x20);
        assert_eq!(c.setup.index, 0);
        assert_eq!(c.setup.length, 7);
        assert_eq!(c.data, vec![0x00, 0xc2, 0x01, 0x00, 0x00, 0x00, 0x08]);
    }

    #[test]
    fn line_coding_other_formats_and_interface() {
        let line = Line {
            baud: 9600,
            data_bits: 7,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
        };
        let c = set_line_coding(2, &line);
        assert_eq!(c.setup.index, 2);
        assert_eq!(c.data, vec![0x80, 0x25, 0, 0, 2, 2, 7]);
    }

    #[test]
    fn control_line_state_and_break() {
        assert_eq!(control_line_state(0, true, true).setup.value, 0x0003);
        assert_eq!(control_line_state(0, true, false).setup.value, 0x0001);
        assert_eq!(control_line_state(3, false, true).setup.index, 3);
        assert_eq!(send_break(0, true).setup.value, 0xffff);
        assert_eq!(send_break(0, false).setup.value, 0);
        assert_eq!(send_break(0, true).setup.request, 0x23);
    }

    #[test]
    fn serial_state_dcd_dsr() {
        let n = parse_notifications(&[0xa1, 0x20, 0, 0, 0, 0, 2, 0, 0x03, 0x00]);
        assert_eq!(n.len(), 1);
        let (m, e) = n[0];
        assert_eq!(
            m,
            ModemStatus {
                cts: false,
                dsr: true,
                ri: false,
                dcd: true
            }
        );
        assert!(!e.any());
    }

    #[test]
    fn serial_state_errors_and_two_notifications() {
        let mut data = vec![0xa1, 0x20, 0, 0, 0, 0, 2, 0, 0x08, 0x00];
        data.extend_from_slice(&[0xa1, 0x20, 0, 0, 0, 0, 2, 0, 0x50, 0x00]);
        let n = parse_notifications(&data);
        assert_eq!(n.len(), 2);
        assert!(n[0].0.ri);
        assert!(n[1].1.framing && n[1].1.overrun && !n[1].1.parity);
        // Truncated or foreign notifications are ignored.
        assert!(parse_notifications(&[0xa1, 0x20, 0, 0, 0, 0, 2, 0, 0x03]).is_empty());
        assert!(parse_notifications(&[0xa1, 0x00, 0, 0, 0, 0, 0, 0]).is_empty());
    }
}
