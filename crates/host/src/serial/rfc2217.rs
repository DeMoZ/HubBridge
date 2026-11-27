//! RFC 2217 (telnet COM-PORT-OPTION) as pure functions and state machines: bytes in →
//! events, requests and negotiation answers out. No I/O here; the server is in [`super::tcp`].
//!
//! Telnet framing (RFC 854): `IAC` = 255 doubles as data; commands are `IAC <verb> <option>`
//! (WILL 251, WONT 252, DO 253, DONT 254) and `IAC SB <option> <data> IAC SE`, where 255
//! inside the data is doubled too.

use super::{Flow, ModemStatus, Parity, StopBits};

pub const IAC: u8 = 255;
pub const DONT: u8 = 254;
pub const DO: u8 = 253;
pub const WONT: u8 = 252;
pub const WILL: u8 = 251;
pub const SB: u8 = 250;
pub const SE: u8 = 240;

pub const OPT_BINARY: u8 = 0;
pub const OPT_SGA: u8 = 3;
/// COM-PORT-OPTION.
pub const OPT_COM_PORT: u8 = 44;

// Client → server commands (the server answers with code + 100).
pub const CMD_SIGNATURE: u8 = 0;
pub const CMD_SET_BAUDRATE: u8 = 1;
pub const CMD_SET_DATASIZE: u8 = 2;
pub const CMD_SET_PARITY: u8 = 3;
pub const CMD_SET_STOPSIZE: u8 = 4;
pub const CMD_SET_CONTROL: u8 = 5;
pub const CMD_NOTIFY_LINESTATE: u8 = 6;
pub const CMD_NOTIFY_MODEMSTATE: u8 = 7;
pub const CMD_FLOWCONTROL_SUSPEND: u8 = 8;
pub const CMD_FLOWCONTROL_RESUME: u8 = 9;
pub const CMD_SET_LINESTATE_MASK: u8 = 10;
pub const CMD_SET_MODEMSTATE_MASK: u8 = 11;
pub const CMD_PURGE_DATA: u8 = 12;
/// Added to a client's command code in the server's reply.
pub const REPLY: u8 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Will,
    Wont,
    Do,
    Dont,
}

impl Verb {
    fn byte(self) -> u8 {
        match self {
            Verb::Will => WILL,
            Verb::Wont => WONT,
            Verb::Do => DO,
            Verb::Dont => DONT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Data bytes, 0xff already un-doubled.
    Data(Vec<u8>),
    Negotiate {
        verb: Verb,
        option: u8,
    },
    /// Subnegotiation: option and its data, 0xff un-doubled.
    Sub {
        option: u8,
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PState {
    Data,
    Iac,
    Verb(Verb),
    SbOption,
    SbData,
    SbIac,
}

/// Telnet parser that survives any split of the input.
#[derive(Debug)]
pub struct Parser {
    state: PState,
    option: u8,
    sb: Vec<u8>,
    /// The subnegotiation outgrew [`MAX_SUBNEGOTIATION`] and is dropped at its end.
    sb_overflow: bool,
}

/// Longest subnegotiation kept; a longer one is dropped (a COM-PORT message is a few bytes).
pub const MAX_SUBNEGOTIATION: usize = 256;

impl Default for Parser {
    fn default() -> Self {
        Parser::new()
    }
}

impl Parser {
    pub fn new() -> Parser {
        Parser {
            state: PState::Data,
            option: 0,
            sb: Vec::new(),
            sb_overflow: false,
        }
    }

    fn push_sb(&mut self, b: u8) {
        if self.sb.len() >= MAX_SUBNEGOTIATION {
            self.sb_overflow = true;
        } else {
            self.sb.push(b);
        }
    }

    pub fn feed(&mut self, input: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        let mut data: Vec<u8> = Vec::new();
        fn flush(data: &mut Vec<u8>, events: &mut Vec<Event>) {
            if !data.is_empty() {
                events.push(Event::Data(std::mem::take(data)));
            }
        }
        for &b in input {
            match self.state {
                PState::Data => {
                    if b == IAC {
                        self.state = PState::Iac;
                    } else {
                        data.push(b);
                    }
                }
                PState::Iac => match b {
                    IAC => {
                        data.push(IAC);
                        self.state = PState::Data;
                    }
                    WILL => self.state = PState::Verb(Verb::Will),
                    WONT => self.state = PState::Verb(Verb::Wont),
                    DO => self.state = PState::Verb(Verb::Do),
                    DONT => self.state = PState::Verb(Verb::Dont),
                    SB => self.state = PState::SbOption,
                    // NOP, GA, a stray SE and the like carry nothing for us.
                    _ => self.state = PState::Data,
                },
                PState::Verb(verb) => {
                    flush(&mut data, &mut events);
                    events.push(Event::Negotiate { verb, option: b });
                    self.state = PState::Data;
                }
                PState::SbOption => {
                    flush(&mut data, &mut events);
                    self.option = b;
                    self.sb.clear();
                    self.sb_overflow = false;
                    self.state = PState::SbData;
                }
                PState::SbData => {
                    if b == IAC {
                        self.state = PState::SbIac;
                    } else {
                        self.push_sb(b);
                    }
                }
                PState::SbIac => match b {
                    IAC => {
                        self.push_sb(IAC);
                        self.state = PState::SbData;
                    }
                    SE => {
                        let data = std::mem::take(&mut self.sb);
                        if !std::mem::take(&mut self.sb_overflow) {
                            events.push(Event::Sub {
                                option: self.option,
                                data,
                            });
                        }
                        self.state = PState::Data;
                    }
                    // A command inside a subnegotiation ends it without a result.
                    _ => {
                        self.sb.clear();
                        self.state = PState::Data;
                    }
                },
            }
        }
        flush(&mut data, &mut events);
        events
    }
}

/// Double 0xff.
pub fn escape(data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(data.len() + 4);
    for &b in data {
        v.push(b);
        if b == IAC {
            v.push(IAC);
        }
    }
    v
}

pub fn negotiate(verb: Verb, option: u8) -> Vec<u8> {
    vec![IAC, verb.byte(), option]
}

/// `IAC SB option <data, 0xff doubled> IAC SE`.
pub fn subnegotiation(option: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![IAC, SB, option];
    v.extend(escape(data));
    v.extend_from_slice(&[IAC, SE]);
    v
}

/// A COM-PORT-OPTION message: `command` followed by `args`.
pub fn com_port(command: u8, args: &[u8]) -> Vec<u8> {
    let mut d = vec![command];
    d.extend_from_slice(args);
    subnegotiation(OPT_COM_PORT, &d)
}

// --- option negotiation ---------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct OptState {
    /// We said WILL (or WONT) and the peer has not contradicted it.
    us: bool,
    us_sent: bool,
    them: bool,
    them_sent: bool,
    refused_do: bool,
    refused_will: bool,
}

/// Answers option negotiation without loops: only a change of state is answered.
#[derive(Debug)]
pub struct Negotiator {
    opts: Vec<OptState>,
}

fn supported(option: u8) -> bool {
    matches!(option, OPT_BINARY | OPT_SGA | OPT_COM_PORT)
}

impl Negotiator {
    /// What the server sends on connect: WILL BINARY, DO BINARY, WILL SGA, DO COM-PORT-OPTION
    /// (RFC 2217 §1: the client says WILL, the access server DO).
    pub fn new() -> (Negotiator, Vec<u8>) {
        let mut n = Negotiator {
            opts: vec![OptState::default(); 256],
        };
        let mut out = Vec::new();
        for (verb, opt) in [
            (Verb::Will, OPT_BINARY),
            (Verb::Do, OPT_BINARY),
            (Verb::Will, OPT_SGA),
            (Verb::Do, OPT_COM_PORT),
        ] {
            let s = &mut n.opts[opt as usize];
            match verb {
                Verb::Will => s.us_sent = true,
                _ => s.them_sent = true,
            }
            out.extend(negotiate(verb, opt));
        }
        (n, out)
    }

    /// The bytes to send in answer to the peer's `verb option`, if any.
    pub fn on_negotiate(&mut self, verb: Verb, option: u8) -> Vec<u8> {
        let s = &mut self.opts[option as usize];
        match verb {
            // The peer asks us to enable (or confirms our WILL).
            Verb::Do => {
                if supported(option) {
                    if s.us {
                        return Vec::new();
                    }
                    s.us = true;
                    if s.us_sent {
                        return Vec::new(); // it agreed with our WILL
                    }
                    s.us_sent = true;
                    negotiate(Verb::Will, option)
                } else if s.refused_do {
                    Vec::new()
                } else {
                    s.refused_do = true;
                    negotiate(Verb::Wont, option)
                }
            }
            Verb::Dont => {
                if s.us {
                    s.us = false;
                    negotiate(Verb::Wont, option)
                } else {
                    Vec::new()
                }
            }
            // The peer offers to enable (or confirms our DO).
            Verb::Will => {
                if matches!(option, OPT_BINARY | OPT_SGA | OPT_COM_PORT) {
                    if s.them {
                        return Vec::new();
                    }
                    s.them = true;
                    if s.them_sent {
                        return Vec::new();
                    }
                    s.them_sent = true;
                    negotiate(Verb::Do, option)
                } else if s.refused_will {
                    Vec::new()
                } else {
                    s.refused_will = true;
                    negotiate(Verb::Dont, option)
                }
            }
            Verb::Wont => {
                if s.them {
                    s.them = false;
                    negotiate(Verb::Dont, option)
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// The client agreed to the COM-PORT-OPTION (WILL), or asked for it (DO).
    pub fn com_port_enabled(&self) -> bool {
        let s = &self.opts[OPT_COM_PORT as usize];
        s.them || s.us
    }
}

// --- COM-PORT-OPTION requests and replies -----------------------------------------------------

/// A parsed client request. A value of 0 (baud, data size, parity, stop size) or the "request
/// current" codes of SET-CONTROL ask for the setting in effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Signature(Vec<u8>),
    SetBaudrate(u32),
    SetDatasize(u8),
    SetParity(u8),
    SetStopsize(u8),
    SetControl(u8),
    FlowSuspend,
    FlowResume,
    SetLinestateMask(u8),
    SetModemstateMask(u8),
    PurgeData(u8),
    Unknown(u8),
}

/// Parse the data of a COM-PORT-OPTION subnegotiation (command byte first).
pub fn parse_request(data: &[u8]) -> Option<Request> {
    let (&cmd, args) = data.split_first()?;
    let b = || args.first().copied();
    Some(match cmd {
        CMD_SIGNATURE => Request::Signature(args.to_vec()),
        CMD_SET_BAUDRATE if args.len() >= 4 => {
            Request::SetBaudrate(u32::from_be_bytes([args[0], args[1], args[2], args[3]]))
        }
        CMD_SET_BAUDRATE => return None,
        CMD_SET_DATASIZE => Request::SetDatasize(b()?),
        CMD_SET_PARITY => Request::SetParity(b()?),
        CMD_SET_STOPSIZE => Request::SetStopsize(b()?),
        CMD_SET_CONTROL => Request::SetControl(b()?),
        CMD_FLOWCONTROL_SUSPEND => Request::FlowSuspend,
        CMD_FLOWCONTROL_RESUME => Request::FlowResume,
        CMD_SET_LINESTATE_MASK => Request::SetLinestateMask(b()?),
        CMD_SET_MODEMSTATE_MASK => Request::SetModemstateMask(b()?),
        CMD_PURGE_DATA => Request::PurgeData(b()?),
        other => Request::Unknown(other),
    })
}

pub fn reply_baudrate(baud: u32) -> Vec<u8> {
    com_port(CMD_SET_BAUDRATE + REPLY, &baud.to_be_bytes())
}

/// Reply of a one-byte command: `command + 100` with the value in effect.
pub fn reply_byte(command: u8, value: u8) -> Vec<u8> {
    com_port(command + REPLY, &[value])
}

pub fn reply_signature(text: &str) -> Vec<u8> {
    com_port(CMD_SIGNATURE + REPLY, text.as_bytes())
}

pub fn notify_modemstate(value: u8) -> Vec<u8> {
    com_port(CMD_NOTIFY_MODEMSTATE + REPLY, &[value])
}

pub fn notify_linestate(value: u8) -> Vec<u8> {
    com_port(CMD_NOTIFY_LINESTATE + REPLY, &[value])
}

// --- values -----------------------------------------------------------------------------------

pub fn parity_code(p: Parity) -> u8 {
    match p {
        Parity::None => 1,
        Parity::Odd => 2,
        Parity::Even => 3,
        Parity::Mark => 4,
        Parity::Space => 5,
    }
}

pub fn parity_from_code(c: u8) -> Option<Parity> {
    Some(match c {
        1 => Parity::None,
        2 => Parity::Odd,
        3 => Parity::Even,
        4 => Parity::Mark,
        5 => Parity::Space,
        _ => return None,
    })
}

pub fn stopsize_code(s: StopBits) -> u8 {
    match s {
        StopBits::One => 1,
        StopBits::Two => 2,
        StopBits::OnePointFive => 3,
    }
}

pub fn stopsize_from_code(c: u8) -> Option<StopBits> {
    Some(match c {
        1 => StopBits::One,
        2 => StopBits::Two,
        3 => StopBits::OnePointFive,
        _ => return None,
    })
}

/// SET-CONTROL value of the flow control in effect: 1 none, 3 hardware.
pub fn flow_code(f: Flow) -> u8 {
    match f {
        Flow::None => 1,
        Flow::RtsCts => 3,
    }
}

/// Modem state byte: levels CTS 0x10, DSR 0x20, RI 0x40, DCD 0x80 and the changes since
/// `prev`: delta CTS 0x01, delta DSR 0x02, trailing-edge RI 0x04, delta DCD 0x08.
pub fn modem_state_byte(prev: ModemStatus, now: ModemStatus) -> u8 {
    let mut v = 0u8;
    if now.cts {
        v |= 0x10;
    }
    if now.dsr {
        v |= 0x20;
    }
    if now.ri {
        v |= 0x40;
    }
    if now.dcd {
        v |= 0x80;
    }
    if prev.cts != now.cts {
        v |= 0x01;
    }
    if prev.dsr != now.dsr {
        v |= 0x02;
    }
    if prev.ri && !now.ri {
        v |= 0x04;
    }
    if prev.dcd != now.dcd {
        v |= 0x08;
    }
    v
}

/// Line state byte: overrun 0x02, parity 0x04, framing 0x08, break 0x10.
pub fn line_state_byte(e: super::LineErrors) -> u8 {
    let mut v = 0u8;
    if e.overrun {
        v |= 0x02;
    }
    if e.parity {
        v |= 0x04;
    }
    if e.framing {
        v |= 0x08;
    }
    if e.brk {
        v |= 0x10;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_by_one(p: &mut Parser, bytes: &[u8]) -> Vec<Event> {
        bytes.iter().flat_map(|b| p.feed(&[*b])).collect()
    }

    const SET_BAUD_115200: [u8; 10] = [0xff, 0xfa, 0x2c, 0x01, 0x00, 0x01, 0xc2, 0x00, 0xff, 0xf0];

    #[test]
    fn set_baudrate_in_one_byte_pieces() {
        let mut p = Parser::new();
        let ev = feed_by_one(&mut p, &SET_BAUD_115200);
        assert_eq!(
            ev,
            vec![Event::Sub {
                option: OPT_COM_PORT,
                data: vec![0x01, 0x00, 0x01, 0xc2, 0x00]
            }]
        );
        let Event::Sub { data, .. } = &ev[0] else {
            unreachable!()
        };
        assert_eq!(parse_request(data), Some(Request::SetBaudrate(115_200)));
        // The reply of the spec.
        assert_eq!(
            reply_baudrate(115_200),
            vec![0xff, 0xfa, 0x2c, 0x65, 0x00, 0x01, 0xc2, 0x00, 0xff, 0xf0]
        );
        // Same bytes in one piece, and split in the middle of the framing.
        assert_eq!(Parser::new().feed(&SET_BAUD_115200), ev);
        let mut p = Parser::new();
        let mut ev2 = p.feed(&SET_BAUD_115200[..9]);
        ev2.extend(p.feed(&SET_BAUD_115200[9..]));
        assert_eq!(ev2, ev);
    }

    #[test]
    fn data_is_unescaped_and_escaped() {
        let mut p = Parser::new();
        assert_eq!(
            p.feed(&[b'a', 0xff, 0xff, b'b']),
            vec![Event::Data(vec![b'a', 0xff, b'b'])]
        );
        // A doubled 0xff split across reads.
        let mut p = Parser::new();
        let mut ev = p.feed(&[b'x', 0xff]);
        ev.extend(p.feed(&[0xff, b'y']));
        assert_eq!(ev, vec![Event::Data(vec![b'x']), Event::Data(vec![0xff, b'y'])]);
        assert_eq!(escape(&[1, 0xff, 2]), vec![1, 0xff, 0xff, 2]);
    }

    #[test]
    fn ff_inside_a_subnegotiation_is_doubled_both_ways() {
        // Baud 65280 = 00 00 ff 00.
        let msg = reply_baudrate(65_280);
        assert_eq!(
            msg,
            vec![0xff, 0xfa, 0x2c, 0x65, 0x00, 0x00, 0xff, 0xff, 0x00, 0xff, 0xf0]
        );
        let ev = Parser::new().feed(&msg);
        assert_eq!(
            ev,
            vec![Event::Sub {
                option: 44,
                data: vec![0x65, 0, 0, 0xff, 0]
            }]
        );
        let req = parse_request(&[1, 0, 0, 0xff, 0]);
        assert_eq!(req, Some(Request::SetBaudrate(65_280)));
    }

    #[test]
    fn data_around_commands() {
        let mut p = Parser::new();
        let ev = p.feed(&[b'a', IAC, DO, 44, b'b', IAC, SB, 44, 5, 9, IAC, SE, b'c']);
        assert_eq!(
            ev,
            vec![
                Event::Data(vec![b'a']),
                Event::Negotiate {
                    verb: Verb::Do,
                    option: 44
                },
                Event::Data(vec![b'b']),
                Event::Sub {
                    option: 44,
                    data: vec![5, 9]
                },
                Event::Data(vec![b'c']),
            ]
        );
        // NOP and friends are skipped.
        assert_eq!(Parser::new().feed(&[IAC, 241, b'z']), vec![Event::Data(vec![b'z'])]);
    }

    #[test]
    fn negotiation_does_not_loop() {
        let (mut n, hello) = Negotiator::new();
        assert_eq!(
            hello,
            vec![IAC, WILL, 0, IAC, DO, 0, IAC, WILL, 3, IAC, DO, 44],
            "WILL BINARY, DO BINARY, WILL SGA, DO COM-PORT-OPTION"
        );
        // The client agrees with all of it: no answers. For the COM-PORT-OPTION the client
        // says WILL (RFC 2217 section 1).
        assert!(n.on_negotiate(Verb::Do, OPT_BINARY).is_empty());
        assert!(n.on_negotiate(Verb::Will, OPT_BINARY).is_empty());
        assert!(n.on_negotiate(Verb::Do, OPT_SGA).is_empty());
        assert!(n.on_negotiate(Verb::Will, OPT_COM_PORT).is_empty());
        assert!(n.com_port_enabled());
        assert!(n.on_negotiate(Verb::Will, OPT_COM_PORT).is_empty(), "no loop");
        // The client's own offer of SGA is accepted once.
        assert_eq!(n.on_negotiate(Verb::Will, OPT_SGA), vec![IAC, DO, 3]);
        assert!(n.on_negotiate(Verb::Will, OPT_SGA).is_empty());
    }

    #[test]
    fn client_will_com_port_is_accepted_and_do_is_tolerated() {
        // A client that answers our DO with WILL: nothing more to say, and it works.
        let (mut n, _hello) = Negotiator::new();
        assert!(
            n.on_negotiate(Verb::Will, OPT_COM_PORT).is_empty(),
            "our DO already asked"
        );
        assert!(n.com_port_enabled());
        // A client that sends DO instead (not per the RFC) gets WILL once and works.
        let (mut n, _) = Negotiator::new();
        assert_eq!(n.on_negotiate(Verb::Do, OPT_COM_PORT), vec![IAC, WILL, 44]);
        assert!(n.on_negotiate(Verb::Do, OPT_COM_PORT).is_empty(), "not answered twice");
        assert!(n.com_port_enabled());
        // WONT / DONT turn it off and are confirmed once.
        let (mut n, _) = Negotiator::new();
        n.on_negotiate(Verb::Will, OPT_COM_PORT);
        assert_eq!(n.on_negotiate(Verb::Wont, OPT_COM_PORT), vec![IAC, DONT, 44]);
        assert!(!n.com_port_enabled());
    }

    #[test]
    fn long_subnegotiations_are_dropped() {
        let mut p = Parser::new();
        let mut msg = vec![IAC, SB, 44];
        msg.extend(std::iter::repeat_n(7u8, MAX_SUBNEGOTIATION + 10));
        msg.extend_from_slice(&[IAC, SE, b'x']);
        assert_eq!(p.feed(&msg), vec![Event::Data(vec![b'x'])]);
        // Exactly at the limit is kept, and the parser works afterwards.
        let mut ok = vec![IAC, SB, 44];
        ok.extend(std::iter::repeat_n(7u8, MAX_SUBNEGOTIATION));
        ok.extend_from_slice(&[IAC, SE]);
        assert_eq!(p.feed(&ok).len(), 1);
        assert_eq!(p.feed(&[IAC, SB, 44, 1, IAC, SE]).len(), 1);
    }

    #[test]
    fn unknown_options_are_refused_once() {
        let (mut n, _) = Negotiator::new();
        assert_eq!(n.on_negotiate(Verb::Do, 24), vec![IAC, WONT, 24]);
        assert!(n.on_negotiate(Verb::Do, 24).is_empty());
        assert_eq!(n.on_negotiate(Verb::Will, 31), vec![IAC, DONT, 31]);
        assert!(n.on_negotiate(Verb::Will, 31).is_empty());
        // DONT of something never enabled: nothing to say.
        assert!(n.on_negotiate(Verb::Dont, 24).is_empty());
        assert!(n.on_negotiate(Verb::Wont, 31).is_empty());
        // DONT of an enabled option is confirmed.
        n.on_negotiate(Verb::Do, OPT_COM_PORT);
        assert_eq!(n.on_negotiate(Verb::Dont, OPT_COM_PORT), vec![IAC, WONT, 44]);
        assert!(!n.com_port_enabled());
    }

    #[test]
    fn every_request_parses() {
        assert_eq!(parse_request(&[0]), Some(Request::Signature(vec![])));
        assert_eq!(parse_request(&[0, b'a']), Some(Request::Signature(vec![b'a'])));
        assert_eq!(parse_request(&[1, 0, 0, 0, 0]), Some(Request::SetBaudrate(0)));
        assert_eq!(parse_request(&[2, 8]), Some(Request::SetDatasize(8)));
        assert_eq!(parse_request(&[3, 3]), Some(Request::SetParity(3)));
        assert_eq!(parse_request(&[4, 2]), Some(Request::SetStopsize(2)));
        assert_eq!(parse_request(&[5, 9]), Some(Request::SetControl(9)));
        assert_eq!(parse_request(&[8]), Some(Request::FlowSuspend));
        assert_eq!(parse_request(&[9]), Some(Request::FlowResume));
        assert_eq!(parse_request(&[10, 0x1a]), Some(Request::SetLinestateMask(0x1a)));
        assert_eq!(parse_request(&[11, 0xff]), Some(Request::SetModemstateMask(0xff)));
        assert_eq!(parse_request(&[12, 3]), Some(Request::PurgeData(3)));
        assert_eq!(parse_request(&[77, 1]), Some(Request::Unknown(77)));
        assert_eq!(parse_request(&[1, 0, 1]), None, "short baud");
        assert_eq!(parse_request(&[]), None);
        assert_eq!(parse_request(&[2]), None);
    }

    #[test]
    fn replies_and_notifications() {
        assert_eq!(reply_byte(CMD_SET_DATASIZE, 8), vec![IAC, SB, 44, 102, 8, IAC, SE]);
        assert_eq!(reply_byte(CMD_SET_CONTROL, 9), vec![IAC, SB, 44, 105, 9, IAC, SE]);
        assert_eq!(notify_modemstate(0x88), vec![IAC, SB, 44, 107, 0x88, IAC, SE]);
        assert_eq!(notify_linestate(0x08), vec![IAC, SB, 44, 106, 0x08, IAC, SE]);
        assert_eq!(notify_modemstate(0xff), vec![IAC, SB, 44, 107, 0xff, 0xff, IAC, SE]);
        assert_eq!(
            reply_signature("HubBridge x"),
            [&[IAC, SB, 44, 100][..], b"HubBridge x", &[IAC, SE]].concat()
        );
    }

    #[test]
    fn value_codes() {
        for p in [Parity::None, Parity::Odd, Parity::Even, Parity::Mark, Parity::Space] {
            assert_eq!(parity_from_code(parity_code(p)), Some(p));
        }
        assert_eq!(parity_from_code(0), None);
        for s in [StopBits::One, StopBits::Two, StopBits::OnePointFive] {
            assert_eq!(stopsize_from_code(stopsize_code(s)), Some(s));
        }
        assert_eq!((flow_code(Flow::None), flow_code(Flow::RtsCts)), (1, 3));
    }

    #[test]
    fn modem_and_line_state_bytes() {
        let off = ModemStatus::default();
        let dcd = ModemStatus { dcd: true, ..off };
        assert_eq!(modem_state_byte(off, dcd), 0x88, "DCD level and delta DCD");
        assert_eq!(modem_state_byte(dcd, dcd), 0x80);
        let ri = ModemStatus { ri: true, ..off };
        assert_eq!(modem_state_byte(ri, off), 0x04, "trailing edge of RI");
        assert_eq!(modem_state_byte(off, ri), 0x40);
        let cts_dsr = ModemStatus {
            cts: true,
            dsr: true,
            ..off
        };
        assert_eq!(modem_state_byte(off, cts_dsr), 0x33);
        let e = super::super::LineErrors {
            framing: true,
            overrun: true,
            ..Default::default()
        };
        assert_eq!(line_state_byte(e), 0x0a);
    }
}
