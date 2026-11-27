//! WCH CH340/CH341 (Linux `ch341.c`): vendor requests to the device.

use super::{Ctl, Line, ModemStatus, Parity, StopBits};

const OUT: u8 = 0x40;
const IN: u8 = 0xc0;

pub const READ_VERSION: u8 = 0x5f;
pub const WRITE_REG: u8 = 0x9a;
pub const READ_REG: u8 = 0x95;
pub const SERIAL_INIT: u8 = 0xa1;
pub const MODEM_CTRL: u8 = 0xa4;

const REG_BREAK: u16 = 0x1805;
const REG_BAUD: u16 = 0x1312; // DIVISOR << 8 | PRESCALER
const REG_LCR: u16 = 0x2518; // LCR2 << 8 | LCR
const REG_MODEM_STATUS: u16 = 0x0706;

const BIT_DTR: u8 = 1 << 5;
const BIT_RTS: u8 = 1 << 6;

const CLKRATE: u64 = 48_000_000;
pub const MIN_BAUD: u32 = 46;
pub const MAX_BAUD: u32 = 3_000_000;

fn clk_div(ps: u32, fact: u32) -> u64 {
    1u64 << (12 - 3 * ps - fact)
}

fn min_rate(ps: u32) -> u64 {
    CLKRATE / (clk_div(ps, 1) * 512)
}

/// Port of `ch341_get_divisor`: the index of WRITE_REG 0x1312 without bit 7.
/// `limited_prescaler` is the quirk found when the break register cannot be read.
pub fn divisor(baud: u32, limited_prescaler: bool) -> Result<u16, String> {
    if !(MIN_BAUD..=MAX_BAUD).contains(&baud) {
        return Err(format!("CH34x supports {MIN_BAUD} to {MAX_BAUD} baud, not {baud}"));
    }
    let speed = baud as u64;
    let mut fact = 1u32;
    let mut ps = 3i32;
    while ps >= 0 {
        if speed > min_rate(ps as u32) {
            break;
        }
        ps -= 1;
    }
    if ps < 0 {
        return Err(format!("CH34x cannot do {baud} baud"));
    }
    let ps = ps as u32;
    let mut cd = clk_div(ps, fact);
    let mut div = CLKRATE / (cd * speed);
    let force_fact0 = ps < 3 && limited_prescaler;
    if !(9..=255).contains(&div) || force_fact0 {
        div /= 2;
        cd *= 2;
        fact = 0;
    }
    if div < 2 {
        return Err(format!("CH34x cannot do {baud} baud"));
    }
    // Pick the next divisor when it gives a closer rate (scaled to avoid rounding errors).
    let here = 16 * CLKRATE as i64 / (cd * div) as i64 - 16 * speed as i64;
    let next = 16 * speed as i64 - 16 * CLKRATE as i64 / (cd * (div + 1)) as i64;
    if here >= next {
        div += 1;
    }
    // Prefer the lower base clock for an even divisor.
    if fact == 1 && div.is_multiple_of(2) {
        div /= 2;
        fact = 0;
    }
    Ok(((0x100 - div as u32) << 8 | fact << 2 | ps) as u16)
}

pub fn read_version() -> Ctl {
    Ctl::read(IN, READ_VERSION, 0, 0, 2)
}

pub fn serial_init() -> Ctl {
    Ctl::out(OUT, SERIAL_INIT, 0, 0, Vec::new())
}

/// The read whose failure marks a chip with a limited prescaler and no break support.
pub fn read_break_reg() -> Ctl {
    Ctl::read(IN, READ_REG, REG_BREAK, 0, 2)
}

/// WRITE_REG 0x1312 with the divisor; bit 7 (send short packets) for versions above 0x27.
pub fn set_baud(version: u8, baud: u32, limited_prescaler: bool) -> Result<Ctl, String> {
    let mut idx = divisor(baud, limited_prescaler)?;
    if version > 0x27 {
        idx |= 0x80;
    }
    Ok(Ctl::out(OUT, WRITE_REG, REG_BAUD, idx, Vec::new()))
}

/// The line format the chip can really do: 5 to 8 data bits, 1 or 2 stop bits.
pub fn effective_line(mut line: Line) -> Line {
    if !(5..=8).contains(&line.data_bits) {
        line.data_bits = 8;
    }
    if line.stop_bits == StopBits::OnePointFive {
        line.stop_bits = StopBits::One;
    }
    line
}

pub fn lcr(line: &Line) -> u8 {
    let mut v = 0x80 | 0x40; // enable RX and TX
    v |= match line.data_bits {
        5 => 0,
        6 => 1,
        7 => 2,
        _ => 3,
    };
    v |= match line.parity {
        Parity::None => 0,
        Parity::Odd => 0x08,
        Parity::Even => 0x08 | 0x10,
        Parity::Mark => 0x08 | 0x20,
        Parity::Space => 0x08 | 0x10 | 0x20,
    };
    if line.stop_bits == StopBits::Two {
        v |= 0x04;
    }
    v
}

/// WRITE_REG 0x2518 with the LCR; versions before 0x30 use other registers and get none.
pub fn set_lcr(version: u8, line: &Line) -> Option<Ctl> {
    (version >= 0x30).then(|| Ctl::out(OUT, WRITE_REG, REG_LCR, lcr(line) as u16, Vec::new()))
}

/// MODEM_CTRL: the value is the inverted line bits.
pub fn modem_ctrl(dtr: bool, rts: bool) -> Ctl {
    let mut bits = 0u8;
    if dtr {
        bits |= BIT_DTR;
    }
    if rts {
        bits |= BIT_RTS;
    }
    Ctl::out(OUT, MODEM_CTRL, !(bits as u16), 0, Vec::new())
}

pub fn read_modem_status() -> Ctl {
    Ctl::read(IN, READ_REG, REG_MODEM_STATUS, 0, 2)
}

fn status_from_inverted(b: u8) -> ModemStatus {
    let s = !b & 0x0f;
    ModemStatus {
        cts: s & 0x01 != 0,
        dsr: s & 0x02 != 0,
        ri: s & 0x04 != 0,
        dcd: s & 0x08 != 0,
    }
}

/// The modem status register (inverted).
pub fn parse_status_reg(data: &[u8]) -> Option<ModemStatus> {
    data.first().map(|b| status_from_inverted(*b))
}

/// An interrupt packet: byte 2 holds the inverted status (`ch341_read_int_callback`).
pub fn parse_interrupt(data: &[u8]) -> Option<ModemStatus> {
    (data.len() > 3).then(|| status_from_inverted(data[2]))
}

/// Break control is read-modify-write of the 2-byte break register: `regs` is what
/// READ_REG returned; the result is the WRITE_REG that sets or clears break.
pub fn break_write(regs: [u8; 2], on: bool) -> Ctl {
    let mut r = regs;
    if on {
        r[0] &= !0x01; // NBREAK
        r[1] &= !0x40; // LCR TX enable
    } else {
        r[0] |= 0x01;
        r[1] |= 0x40;
    }
    Ctl::out(OUT, WRITE_REG, REG_BREAK, u16::from_le_bytes(r), Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(c: Ctl) -> u16 {
        assert_eq!(
            (c.setup.request_type, c.setup.request, c.setup.value),
            (0x40, 0x9a, 0x1312)
        );
        c.setup.index
    }

    #[test]
    fn divisors_of_version_0x31() {
        assert_eq!(idx(set_baud(0x31, 115_200, false).unwrap()), 0xcc83);
        assert_eq!(idx(set_baud(0x31, 9600, false).unwrap()), 0xb282);
        assert_eq!(idx(set_baud(0x31, 12_000, false).unwrap()), 0x8386);
    }

    #[test]
    fn limited_prescaler_forces_fact0() {
        assert_eq!(idx(set_baud(0x31, 12_000, true).unwrap()), 0xc182);
        // ps = 3 is unaffected by the quirk.
        assert_eq!(idx(set_baud(0x31, 115_200, true).unwrap()), 0xcc83);
    }

    #[test]
    fn version_0x27_has_no_bit7_and_no_lcr() {
        assert_eq!(idx(set_baud(0x27, 115_200, false).unwrap()), 0xcc03);
        assert!(set_lcr(0x27, &Line::default()).is_none());
        assert!(set_lcr(0x2f, &Line::default()).is_none());
        assert!(set_lcr(0x30, &Line::default()).is_some());
    }

    #[test]
    fn lcr_values() {
        let c = set_lcr(0x31, &Line::default()).unwrap();
        assert_eq!((c.setup.request, c.setup.value, c.setup.index), (0x9a, 0x2518, 0x00c3));
        let even7 = Line {
            data_bits: 7,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            ..Line::default()
        };
        assert_eq!(lcr(&even7), 0xc0 | 0x02 | 0x18 | 0x04);
        let five = Line {
            data_bits: 5,
            ..Line::default()
        };
        assert_eq!(lcr(&five), 0xc0);
    }

    #[test]
    fn modem_control_is_inverted() {
        assert_eq!(modem_ctrl(true, true).setup.value, 0xff9f);
        assert_eq!(modem_ctrl(false, false).setup.value, 0xffff);
        assert_eq!(modem_ctrl(true, false).setup.value, 0xffdf);
        assert_eq!(modem_ctrl(true, true).setup.request, 0xa4);
    }

    #[test]
    fn out_of_range_rates_are_refused() {
        assert!(divisor(45, false).is_err());
        assert!(divisor(3_000_001, false).is_err());
        assert!(divisor(46, false).is_ok());
        assert!(divisor(3_000_000, false).is_ok());
        let e = set_baud(0x31, 45, false).unwrap_err();
        assert!(e.contains("46"), "{e}");
    }

    #[test]
    fn status_parsers() {
        let s = parse_interrupt(&[0, 0, !0x09u8, 0]).unwrap();
        assert!(s.cts && s.dcd && !s.dsr && !s.ri);
        assert!(parse_interrupt(&[0, 0, 0xff]).is_none());
        assert_eq!(parse_status_reg(&[0xff, 0]), Some(ModemStatus::default()));
        assert!(parse_status_reg(&[0xfd, 0]).unwrap().dsr);
        assert!(parse_status_reg(&[]).is_none());
    }

    #[test]
    fn break_read_modify_write() {
        let on = break_write([0x01, 0xc3], true);
        assert_eq!((on.setup.value, on.setup.index), (0x1805, 0x8300));
        let off = break_write([0x00, 0x83], false);
        assert_eq!(off.setup.index, 0xc301);
    }

    #[test]
    fn extremes() {
        // The highest rate: ps 3, fact 0, divisor 2.
        assert_eq!(idx(set_baud(0x31, 3_000_000, false).unwrap()), 0xfe83);
        assert!(divisor(46, false).is_ok());
    }
}
