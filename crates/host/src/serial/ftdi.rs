//! FTDI (Linux `ftdi_sio.c`): vendor requests, divisor math, 2-byte status header on IN.

use super::{Ctl, Flow, Line, LineErrors, ModemStatus, Parity, StopBits};

const OUT: u8 = 0x40;
const IN: u8 = 0xc0;

pub const RESET: u8 = 0x00;
pub const MODEM_CTRL: u8 = 0x01;
pub const SET_FLOW_CTRL: u8 = 0x02;
pub const SET_BAUD_RATE: u8 = 0x03;
pub const SET_DATA: u8 = 0x04;
pub const SET_LATENCY_TIMER: u8 = 0x09;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtdiType {
    Bm,
    C2232,
    R232,
    H2232,
    H4232,
    H232,
    X,
}

impl FtdiType {
    /// By `bcdDevice` (`ftdi_determine_type`); older chips are not supported.
    pub fn from_bcd(bcd_device: u16) -> Option<FtdiType> {
        Some(match bcd_device {
            0x0400 => FtdiType::Bm,
            0x0500 => FtdiType::C2232,
            0x0600 => FtdiType::R232,
            0x0700 => FtdiType::H2232,
            0x0800 => FtdiType::H4232,
            0x0900 => FtdiType::H232,
            0x1000 => FtdiType::X,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            FtdiType::Bm => "FT232BM",
            FtdiType::C2232 => "FT2232C",
            FtdiType::R232 => "FT232R",
            FtdiType::H2232 => "FT2232H",
            FtdiType::H4232 => "FT4232H",
            FtdiType::H232 => "FT232H",
            FtdiType::X => "FT-X",
        }
    }

    /// High-speed chips use the 120 MHz divisor at 1200 baud and above.
    pub fn high_speed(self) -> bool {
        matches!(self, FtdiType::H2232 | FtdiType::H4232 | FtdiType::H232)
    }

    /// FT232BM and FT232R address no channel (index 0); all others use `1 + interface`.
    pub fn channel(self, interface: u8) -> u16 {
        match self {
            FtdiType::Bm | FtdiType::R232 => 0,
            _ => 1 + interface as u16,
        }
    }

    pub fn max_baud(self) -> u32 {
        if self.high_speed() { 12_000_000 } else { 3_000_000 }
    }
}

/// Lowest rate the divisor can express (24 MHz / 16383 / 8, rounded up).
pub const MIN_BAUD: u32 = 184;

const DIVFRAC: [u32; 8] = [0, 3, 2, 4, 1, 5, 6, 7];

fn round_closest(a: u64, b: u64) -> u64 {
    (a + b / 2) / b
}

/// `ftdi_232bm_baud_base_to_divisor` with the 48 MHz base.
pub fn divisor_232bm(baud: u32) -> u32 {
    let divisor3 = round_closest(48_000_000, 2 * baud as u64) as u32;
    let mut divisor = divisor3 >> 3;
    divisor |= DIVFRAC[(divisor3 & 7) as usize] << 14;
    if divisor == 1 {
        0
    } else if divisor == 0x4001 {
        1
    } else {
        divisor
    }
}

/// `ftdi_2232h_baud_base_to_divisor` with the 120 MHz base: 10-bit sampling, bit 17 set.
pub fn divisor_2232h(baud: u32) -> u32 {
    let divisor3 = round_closest(8 * 120_000_000, 10 * baud as u64) as u32;
    let mut divisor = divisor3 >> 3;
    divisor |= DIVFRAC[(divisor3 & 7) as usize] << 14;
    if divisor == 1 {
        divisor = 0;
    } else if divisor == 0x4001 {
        divisor = 1;
    }
    divisor | 0x0002_0000
}

/// The encoded divisor for a chip and rate (`get_ftdi_divisor`), or why the rate is refused.
pub fn divisor(ty: FtdiType, baud: u32) -> Result<u32, String> {
    let max = ty.max_baud();
    if baud > max {
        return Err(format!("{} supports at most {max} baud, not {baud}", ty.name()));
    }
    if baud < MIN_BAUD {
        return Err(format!("{} supports at least {MIN_BAUD} baud, not {baud}", ty.name()));
    }
    Ok(if ty.high_speed() && baud >= 1200 {
        divisor_2232h(baud)
    } else {
        divisor_232bm(baud)
    })
}

/// SET_BAUD_RATE: value = divisor low 16 bits, index = divisor >> 16, with the channel in
/// the low byte (`change_speed`).
pub fn set_baud(ty: FtdiType, interface: u8, baud: u32) -> Result<Ctl, String> {
    let div = divisor(ty, baud)?;
    let channel = ty.channel(interface);
    let mut index = (div >> 16) as u16;
    if channel != 0 {
        index = (index << 8) | channel;
    }
    Ok(Ctl::out(OUT, SET_BAUD_RATE, div as u16, index, Vec::new()))
}

/// The line format the chip can really do: 7 or 8 data bits (anything else is reported as 8)
/// and 1 or 2 stop bits (1.5 becomes 1).
pub fn effective_line(mut line: Line) -> Line {
    if line.data_bits != 7 {
        line.data_bits = 8;
    }
    if line.stop_bits == StopBits::OnePointFive {
        line.stop_bits = StopBits::One;
    }
    line
}

/// SET_DATA value: data bits, parity at bit 8, stop bits at bit 11, break at bit 14.
pub fn data_value(line: &Line, brk: bool) -> u16 {
    let parity = match line.parity {
        Parity::None => 0u16,
        Parity::Odd => 1,
        Parity::Even => 2,
        Parity::Mark => 3,
        Parity::Space => 4,
    };
    let stop = match line.stop_bits {
        StopBits::Two => 2u16,
        StopBits::OnePointFive => 1,
        StopBits::One => 0,
    };
    line.data_bits as u16 | parity << 8 | stop << 11 | (brk as u16) << 14
}

pub fn set_data(ty: FtdiType, interface: u8, line: &Line, brk: bool) -> Ctl {
    Ctl::out(OUT, SET_DATA, data_value(line, brk), ty.channel(interface), Vec::new())
}

/// MODEM_CTRL writes one line per request (DTR 0x0101 / 0x0100, RTS 0x0202 / 0x0200).
pub fn modem_ctrl(ty: FtdiType, interface: u8, dtr: Option<bool>, rts: Option<bool>) -> Vec<Ctl> {
    let channel = ty.channel(interface);
    let mut v = Vec::new();
    if let Some(on) = dtr {
        v.push(Ctl::out(OUT, MODEM_CTRL, 0x0100 | on as u16, channel, Vec::new()));
    }
    if let Some(on) = rts {
        v.push(Ctl::out(
            OUT,
            MODEM_CTRL,
            0x0200 | (on as u16) << 1,
            channel,
            Vec::new(),
        ));
    }
    v
}

pub fn set_flow_ctrl(ty: FtdiType, interface: u8, flow: Flow) -> Ctl {
    let hs = if flow == Flow::RtsCts { 0x0100 } else { 0 };
    Ctl::out(OUT, SET_FLOW_CTRL, 0, hs | ty.channel(interface), Vec::new())
}

/// RESET value: 0 resets the port, 1 purges RX, 2 purges TX.
pub fn reset(ty: FtdiType, interface: u8, what: u16) -> Ctl {
    Ctl::out(OUT, RESET, what, ty.channel(interface), Vec::new())
}

pub fn set_latency_timer(ty: FtdiType, interface: u8, ms: u8) -> Ctl {
    Ctl::out(OUT, SET_LATENCY_TIMER, ms as u16, ty.channel(interface), Vec::new())
}

pub fn get_modem_status(ty: FtdiType, interface: u8) -> Ctl {
    Ctl::read(IN, 0x05, 0, ty.channel(interface), 2)
}

pub fn parse_modem(b0: u8) -> ModemStatus {
    ModemStatus {
        cts: b0 & 0x10 != 0,
        dsr: b0 & 0x20 != 0,
        ri: b0 & 0x40 != 0,
        dcd: b0 & 0x80 != 0,
    }
}

/// An IN transfer cut into its max-packet chunks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Split {
    /// Data bytes, headers removed.
    pub data: Vec<u8>,
    /// Modem lines of the last packet.
    pub modem: Option<ModemStatus>,
    /// Line errors of the packets that carry data (like `ftdi_process_packet`, status-only
    /// packets do not report errors).
    pub errors: LineErrors,
}

/// Strip the 2 status bytes from every `max_packet`-sized chunk of `buf`.
pub fn split_in(buf: &[u8], max_packet: usize) -> Split {
    let mut out = Split::default();
    for chunk in buf.chunks(max_packet.max(2)) {
        if chunk.len() < 2 {
            continue;
        }
        out.modem = Some(parse_modem(chunk[0]));
        if chunk.len() > 2 {
            out.data.extend_from_slice(&chunk[2..]);
            let l = chunk[1];
            out.errors.overrun |= l & 0x02 != 0;
            out.errors.parity |= l & 0x04 != 0;
            out.errors.framing |= l & 0x08 != 0;
            out.errors.brk |= l & 0x10 != 0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baud(ty: FtdiType, interface: u8, baud: u32) -> (u16, u16) {
        let c = set_baud(ty, interface, baud).unwrap();
        assert_eq!((c.setup.request_type, c.setup.request), (0x40, 0x03));
        (c.setup.value, c.setup.index)
    }

    #[test]
    fn ft232r_divisors() {
        assert_eq!(baud(FtdiType::R232, 0, 9600), (0x4138, 0));
        assert_eq!(baud(FtdiType::R232, 0, 115_200), (0x001a, 0));
        assert_eq!(baud(FtdiType::R232, 0, 921_600), (0x8003, 0));
        assert_eq!(baud(FtdiType::R232, 0, 3_000_000), (0, 0));
    }

    #[test]
    fn high_speed_divisors() {
        // Linux gives the FT232H channel 1 (CHANNEL_A + interface), so the index low byte
        // is 1: the design's 0x0200 had no channel.
        assert_eq!(baud(FtdiType::H232, 0, 3_000_000), (0x0004, 0x0201));
        assert_eq!(baud(FtdiType::H232, 0, 12_000_000), (0x0000, 0x0201));
        // Port B of an FT2232H: interface 1 is channel 2.
        assert_eq!(baud(FtdiType::H2232, 1, 115_200), (0xc068, 0x0202));
        assert_eq!(baud(FtdiType::H2232, 0, 115_200), (0xc068, 0x0201));
        // Below 1200 baud the 48 MHz divisor is used, without the high-speed bit.
        let (v, i) = baud(FtdiType::H232, 0, 300);
        assert_eq!(i, 0x0001);
        assert_eq!(v, divisor_232bm(300) as u16);
    }

    #[test]
    fn rates_out_of_range() {
        assert!(set_baud(FtdiType::R232, 0, 3_000_001).unwrap_err().contains("3000000"));
        assert!(
            set_baud(FtdiType::H232, 0, 12_000_001)
                .unwrap_err()
                .contains("12000000")
        );
        assert!(set_baud(FtdiType::H232, 0, 12_000_000).is_ok());
        assert!(set_baud(FtdiType::R232, 0, 50).is_err());
    }

    #[test]
    fn chip_types() {
        assert_eq!(FtdiType::from_bcd(0x0600), Some(FtdiType::R232));
        assert_eq!(FtdiType::from_bcd(0x0900).unwrap().name(), "FT232H");
        assert_eq!(FtdiType::from_bcd(0x0200), None);
        assert_eq!(FtdiType::R232.channel(0), 0);
        assert_eq!(FtdiType::H2232.channel(1), 2);
        assert_eq!(FtdiType::X.channel(0), 1);
    }

    #[test]
    fn data_bits_parity_stop() {
        let l = Line {
            baud: 9600,
            data_bits: 7,
            parity: Parity::Even,
            stop_bits: StopBits::One,
        };
        assert_eq!(data_value(&l, false), 0x0207);
        assert_eq!(data_value(&Line::default(), false), 0x0008);
        assert_eq!(data_value(&Line::default(), true), 0x4008);
        let two = Line {
            stop_bits: StopBits::Two,
            parity: Parity::Space,
            ..Line::default()
        };
        assert_eq!(data_value(&two, false), 0x1408);
    }

    #[test]
    fn five_data_bits_are_reported_as_eight() {
        let five = Line {
            data_bits: 5,
            ..Line::default()
        };
        assert_eq!(effective_line(five).data_bits, 8);
        let seven = Line {
            data_bits: 7,
            stop_bits: StopBits::OnePointFive,
            ..Line::default()
        };
        let e = effective_line(seven);
        assert_eq!((e.data_bits, e.stop_bits), (7, StopBits::One));
    }

    #[test]
    fn modem_ctrl_values_and_index() {
        let v = modem_ctrl(FtdiType::R232, 0, Some(false), None);
        assert_eq!(v.len(), 1);
        assert_eq!(
            (v[0].setup.request, v[0].setup.value, v[0].setup.index),
            (0x01, 0x0100, 0)
        );
        let v = modem_ctrl(FtdiType::H2232, 1, Some(true), Some(true));
        assert_eq!(
            v.iter().map(|c| c.setup.value).collect::<Vec<_>>(),
            vec![0x0101, 0x0202]
        );
        assert!(v.iter().all(|c| c.setup.index == 2));
        let v = modem_ctrl(FtdiType::R232, 0, None, Some(false));
        assert_eq!(v[0].setup.value, 0x0200);
    }

    #[test]
    fn flow_reset_latency() {
        assert_eq!(set_flow_ctrl(FtdiType::R232, 0, Flow::RtsCts).setup.index, 0x0100);
        assert_eq!(set_flow_ctrl(FtdiType::H2232, 1, Flow::RtsCts).setup.index, 0x0102);
        assert_eq!(set_flow_ctrl(FtdiType::H2232, 1, Flow::None).setup.index, 0x0002);
        assert_eq!(
            (
                reset(FtdiType::R232, 0, 1).setup.request,
                reset(FtdiType::R232, 0, 1).setup.value
            ),
            (0, 1)
        );
        let l = set_latency_timer(FtdiType::H232, 0, 16);
        assert_eq!((l.setup.request, l.setup.value, l.setup.index), (0x09, 16, 1));
    }

    #[test]
    fn two_512_byte_packets_give_1020_bytes() {
        let mut buf = Vec::new();
        for p in 0..2u8 {
            buf.extend_from_slice(&[0x10, 0x60]);
            buf.extend(std::iter::repeat_n(p + 1, 510));
        }
        let s = split_in(&buf, 512);
        assert_eq!(s.data.len(), 1020);
        assert_eq!(s.data[0], 1);
        assert_eq!(s.data[1019], 2);
        assert!(s.modem.unwrap().cts);
        assert!(!s.errors.any());
    }

    #[test]
    fn status_only_packets_deliver_no_data() {
        let s = split_in(&[0x20, 0x60], 64);
        assert!(s.data.is_empty());
        assert!(s.modem.unwrap().dsr);
        // An overrun bit on a status-only packet is not counted; on a data packet it is.
        assert!(!split_in(&[0x00, 0x62], 64).errors.any());
        let s = split_in(&[0x00, 0x62, b'x'], 64);
        assert!(s.errors.overrun);
        assert_eq!(s.data, b"x");
        assert_eq!(split_in(&[0x00], 64), Split::default());
    }

    #[test]
    fn header_split_follows_max_packet() {
        // 64-byte packets: a 130-byte transfer is 64 + 64 + 2.
        let mut buf = vec![0u8; 130];
        for (i, c) in buf.chunks_mut(64).enumerate() {
            c[0] = 0;
            c[1] = 0x60;
            for b in c.iter_mut().skip(2) {
                *b = i as u8 + 10;
            }
        }
        let s = split_in(&buf, 64);
        assert_eq!(s.data.len(), 62 + 62);
    }
}
