//! USB serial ports on the phone, driven from the desktop over the generic USB transfers.
//!
//! Chip logic (CDC-ACM, CP210x, FTDI, CH34x) lives here, on the host: every driver is a set
//! of pure encoders and parsers ([`cdc_acm`], [`cp210x`], [`ftdi`], [`ch34x`]) that return
//! literal control requests, plus the async open sequence and the data paths in [`port`].
//! Front ends ([`pty`], [`tcp`] with [`rfc2217`]) and [`bridge`] build on [`port::SerialPort`].

pub mod bench;
pub mod bridge;
pub mod cdc_acm;
pub mod ch34x;
pub mod cp210x;
pub mod detect;
pub mod ftdi;
pub mod port;
#[cfg(unix)]
pub mod pty;
pub mod rfc2217;
pub mod tcp;

use hubbridge_proto::Setup;

pub use detect::{Detected, DriverKind, NotSerial, detect};
pub use port::{PortInfo, PortOptions, SerialPort};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
    Mark,
    Space,
}

impl Parity {
    /// Code shared by CDC-ACM, CP210x and FTDI: 0 none, 1 odd, 2 even, 3 mark, 4 space.
    pub fn code(self) -> u8 {
        match self {
            Parity::None => 0,
            Parity::Odd => 1,
            Parity::Even => 2,
            Parity::Mark => 3,
            Parity::Space => 4,
        }
    }

    pub fn letter(self) -> char {
        match self {
            Parity::None => 'N',
            Parity::Odd => 'O',
            Parity::Even => 'E',
            Parity::Mark => 'M',
            Parity::Space => 'S',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopBits {
    #[default]
    One,
    OnePointFive,
    Two,
}

impl StopBits {
    /// Code shared by CDC-ACM, CP210x and FTDI: 0 = 1, 1 = 1.5, 2 = 2.
    pub fn code(self) -> u8 {
        match self {
            StopBits::One => 0,
            StopBits::OnePointFive => 1,
            StopBits::Two => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StopBits::One => "1",
            StopBits::OnePointFive => "1.5",
            StopBits::Two => "2",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flow {
    #[default]
    None,
    RtsCts,
}

/// Line format: what a chip is told about the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: StopBits,
}

impl Default for Line {
    fn default() -> Self {
        Line {
            baud: 115_200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
        }
    }
}

impl std::fmt::Display for Line {
    /// `115200 8N1`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}{}{}",
            self.baud,
            self.data_bits,
            self.parity.letter(),
            self.stop_bits.label()
        )
    }
}

/// What to open a port with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialConfig {
    pub line: Line,
    pub flow: Flow,
    pub dtr: bool,
    pub rts: bool,
}

impl Default for SerialConfig {
    fn default() -> Self {
        SerialConfig {
            line: Line::default(),
            flow: Flow::None,
            dtr: true,
            rts: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModemStatus {
    pub cts: bool,
    pub dsr: bool,
    pub ri: bool,
    pub dcd: bool,
}

/// Line errors reported at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineErrors {
    pub overrun: bool,
    pub parity: bool,
    pub framing: bool,
    pub brk: bool,
}

impl LineErrors {
    pub fn any(&self) -> bool {
        self.overrun || self.parity || self.framing || self.brk
    }
}

/// Counters while the port runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub tx: u64,
    pub rx: u64,
    pub rx_dropped: u64,
    pub overruns: u64,
    pub parity: u64,
    pub framing: u64,
    pub breaks: u64,
}

impl Stats {
    pub fn line_errors(&self) -> u64 {
        self.overruns + self.parity + self.framing + self.breaks
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortEvent {
    Modem(ModemStatus),
    LineError(LineErrors),
}

/// One control request: SETUP and, for OUT requests, the data stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ctl {
    pub setup: Setup,
    pub data: Vec<u8>,
}

impl Ctl {
    /// Host-to-device request; `setup.length` is the data length.
    pub fn out(request_type: u8, request: u8, value: u16, index: u16, data: Vec<u8>) -> Ctl {
        Ctl {
            setup: Setup {
                request_type,
                request,
                value,
                index,
                length: data.len() as u16,
            },
            data,
        }
    }

    /// Device-to-host request reading `length` bytes.
    pub fn read(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Ctl {
        Ctl {
            setup: Setup {
                request_type,
                request,
                value,
                index,
                length,
            },
            data: Vec::new(),
        }
    }
}
