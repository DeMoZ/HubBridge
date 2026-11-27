//! Which serial driver fits a USB device, from its descriptors.
//!
//! 1. VID/PID tables for CP210x, FTDI and CH34x, with the first vendor-specific interface
//!    that has a bulk IN and OUT pair (`--port` picks the Nth).
//! 2. Otherwise CDC-ACM: a control interface (class 02, subclass 02) paired with a data
//!    interface (class 0a) the way `cdc-acm.c` does it: union descriptor, then call
//!    management, then the next data interface.

use hubbridge_proto::usb::{InterfaceInfo, TransferType};
use hubbridge_proto::{EndpointInfo, UsbDeviceInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    CdcAcm,
    Cp210x,
    Ftdi,
    Ch34x,
}

impl DriverKind {
    pub fn name(self) -> &'static str {
        match self {
            DriverKind::CdcAcm => "CDC-ACM",
            DriverKind::Cp210x => "CP210x",
            DriverKind::Ftdi => "FTDI",
            DriverKind::Ch34x => "CH34x",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub kind: DriverKind,
    /// Chip name when the descriptors tell it (FTDI by bcdDevice).
    pub chip: Option<String>,
    /// Interface the control requests address: the CDC control interface, otherwise the
    /// interface of the port.
    pub control_interface: u8,
    /// The data interface (CDC); the same as `control_interface` for the vendor chips.
    pub data_interface: u8,
    /// Alternate setting of the data interface that has the bulk endpoints.
    pub data_alternate: u8,
    /// Interfaces to claim, in order.
    pub claim: Vec<u8>,
    pub bulk_in: u8,
    pub bulk_out: u8,
    pub max_packet_in: u16,
    pub max_packet_out: u16,
    /// Interrupt IN endpoint and its max packet size (CDC notifications, CH34x status).
    pub interrupt_in: Option<(u8, u16)>,
    /// CDC ACM `bmCapabilities` (bit 1 line coding, bit 2 send break); 0 when unknown.
    pub acm_capabilities: u8,
    pub bcd_device: u16,
    /// How many ports the device offers (multi-port FTDI, dual CDC).
    pub ports: usize,
}

impl Detected {
    /// `FTDI (FT232R)` or `CP210x`.
    pub fn label(&self) -> String {
        match &self.chip {
            Some(c) => format!("{} ({c})", self.kind.name()),
            None => self.kind.name().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotSerial {
    /// Nothing recognised and the descriptors are hidden because there is no permission.
    NoPermission,
    Unsupported,
    NoSuchPort {
        port: usize,
        ports: usize,
    },
    Other(String),
}

impl std::fmt::Display for NotSerial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotSerial::NoPermission => f.write_str(
                "cannot tell what this device is without its descriptors: run `hubbridge grant <id>` and tap Allow on the phone",
            ),
            NotSerial::Unsupported => {
                f.write_str("not a supported USB serial device (CDC-ACM, CP210x, FTDI, CH34x)")
            }
            NotSerial::NoSuchPort { port, ports } => {
                write!(f, "the device has {ports} port(s), no port {port}")
            }
            NotSerial::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for NotSerial {}

const CP210X_PIDS: &[u16] = &[0xea60, 0xea61, 0xea63, 0xea70, 0xea71, 0xea7a, 0xea7b];
const FTDI_PIDS: &[u16] = &[0x6001, 0x6010, 0x6011, 0x6014, 0x6015];
const CH34X_PIDS: &[u16] = &[0x7523, 0x7522, 0x5523];

/// FTDI chip name from `bcdDevice` (`ftdi_determine_type`).
pub fn ftdi_chip_name(bcd_device: u16) -> Option<&'static str> {
    Some(match bcd_device {
        0x0400 => "FT232BM",
        0x0500 => "FT2232C",
        0x0600 => "FT232R",
        0x0700 => "FT2232H",
        0x0800 => "FT4232H",
        0x0900 => "FT232H",
        0x1000 => "FT-X",
        _ => return None,
    })
}

fn bulk(i: &InterfaceInfo, input: bool) -> Option<&EndpointInfo> {
    i.endpoints
        .iter()
        .find(|e| e.transfer_type() == TransferType::Bulk && e.is_in() == input)
}

fn interrupt_in(i: &InterfaceInfo) -> Option<&EndpointInfo> {
    i.endpoints
        .iter()
        .find(|e| e.transfer_type() == TransferType::Interrupt && e.is_in())
}

/// CDC functional descriptors of one control interface.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CdcFunction {
    control: u8,
    union_data: Option<u8>,
    call_data: Option<u8>,
    caps: Option<u8>,
}

/// Walk the raw descriptors for the class-specific descriptors the parser skips.
fn cdc_functions(raw: &[u8]) -> Vec<CdcFunction> {
    let mut out: Vec<CdcFunction> = Vec::new();
    let mut pos = raw.first().copied().unwrap_or(0) as usize;
    let mut current: Option<usize> = None;
    while pos + 2 <= raw.len() {
        let len = raw[pos] as usize;
        if len < 2 || pos + len > raw.len() {
            break;
        }
        let d = &raw[pos..pos + len];
        match d[1] {
            0x04 if len >= 9 => {
                current = if d[5] == 0x02 && d[6] == 0x02 && d[3] == 0 {
                    out.push(CdcFunction {
                        control: d[2],
                        ..Default::default()
                    });
                    Some(out.len() - 1)
                } else {
                    None
                };
            }
            0x24 if len >= 4 => {
                if let Some(c) = current {
                    match d[2] {
                        0x01 if len >= 5 => out[c].call_data = Some(d[4]),
                        0x02 => out[c].caps = Some(d[3]),
                        0x06 if len >= 5 => out[c].union_data = Some(d[4]),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        pos += len;
    }
    out
}

fn vendor_ports(info: &UsbDeviceInfo) -> Vec<&InterfaceInfo> {
    let Some(cfg) = info.configurations.first() else {
        return Vec::new();
    };
    cfg.interfaces
        .iter()
        .filter(|i| i.alternate == 0 && i.class == 0xff && bulk(i, true).is_some() && bulk(i, false).is_some())
        .collect()
}

fn detect_vendor(info: &UsbDeviceInfo, port: usize) -> Option<Result<Detected, NotSerial>> {
    let kind = match info.vendor_id {
        0x10c4 if CP210X_PIDS.contains(&info.product_id) => DriverKind::Cp210x,
        0x0403 if FTDI_PIDS.contains(&info.product_id) => DriverKind::Ftdi,
        0x1a86 if CH34X_PIDS.contains(&info.product_id) => DriverKind::Ch34x,
        _ => return None,
    };
    let ports = vendor_ports(info);
    if ports.is_empty() {
        return None;
    }
    let Some(i) = ports.get(port) else {
        return Some(Err(NotSerial::NoSuchPort {
            port,
            ports: ports.len(),
        }));
    };
    let bcd = info.bcd_device.unwrap_or(0);
    let chip = if kind == DriverKind::Ftdi {
        match ftdi_chip_name(bcd) {
            Some(n) => Some(n.to_string()),
            None if info.bcd_device.is_none() => None,
            None => {
                return Some(Err(NotSerial::Other(format!(
                    "this FTDI chip (bcdDevice 0x{bcd:04x}) is not supported: FT232BM and newer only"
                ))));
            }
        }
    } else {
        None
    };
    let (bin, bout) = (bulk(i, true)?, bulk(i, false)?);
    Some(Ok(Detected {
        kind,
        chip,
        control_interface: i.number,
        data_interface: i.number,
        data_alternate: 0,
        claim: vec![i.number],
        bulk_in: bin.address,
        bulk_out: bout.address,
        max_packet_in: bin.max_packet_size,
        max_packet_out: bout.max_packet_size,
        interrupt_in: if kind == DriverKind::Ch34x {
            interrupt_in(i).map(|e| (e.address, e.max_packet_size))
        } else {
            None
        },
        acm_capabilities: 0,
        bcd_device: bcd,
        ports: ports.len(),
    }))
}

fn detect_cdc(info: &UsbDeviceInfo, port: usize) -> Option<Result<Detected, NotSerial>> {
    let cfg = info.configurations.first()?;
    let functions = info.raw_descriptors.as_deref().map(cdc_functions).unwrap_or_default();
    let controls: Vec<&InterfaceInfo> = cfg
        .interfaces
        .iter()
        .filter(|i| i.alternate == 0 && i.class == 0x02 && i.subclass == 0x02)
        .collect();
    // Pair every control interface with its data interface; skip those without one.
    let mut found: Vec<Detected> = Vec::new();
    for ctl in controls {
        let f = functions.iter().find(|f| f.control == ctl.number);
        let wanted = f.and_then(|f| f.union_data.or(f.call_data));
        // Data interface candidates: the named one, else the next class 0a interface.
        let data_number = match wanted {
            Some(n) if cfg.interfaces.iter().any(|i| i.number == n) => Some(n),
            _ => cfg
                .interfaces
                .iter()
                .filter(|i| i.class == 0x0a && i.number > ctl.number)
                .map(|i| i.number)
                .min(),
        };
        let Some(data_number) = data_number else { continue };
        // The alternate setting that has the bulk pair.
        let Some(data) = cfg
            .interfaces
            .iter()
            .filter(|i| i.number == data_number)
            .find(|i| bulk(i, true).is_some() && bulk(i, false).is_some())
        else {
            continue;
        };
        let (bin, bout) = (bulk(data, true)?, bulk(data, false)?);
        found.push(Detected {
            kind: DriverKind::CdcAcm,
            chip: None,
            control_interface: ctl.number,
            data_interface: data_number,
            data_alternate: data.alternate,
            claim: vec![ctl.number, data_number],
            bulk_in: bin.address,
            bulk_out: bout.address,
            max_packet_in: bin.max_packet_size,
            max_packet_out: bout.max_packet_size,
            interrupt_in: interrupt_in(ctl).map(|e| (e.address, e.max_packet_size)),
            acm_capabilities: f.and_then(|f| f.caps).unwrap_or(0),
            bcd_device: info.bcd_device.unwrap_or(0),
            ports: 0,
        });
    }
    let n = found.len();
    if n == 0 {
        return None;
    }
    match found.into_iter().nth(port) {
        Some(mut d) => {
            d.ports = n;
            Some(Ok(d))
        }
        None => Some(Err(NotSerial::NoSuchPort { port, ports: n })),
    }
}

/// Pick the driver for `info`, port number `port` (0 = the first).
pub fn detect(info: &UsbDeviceInfo, port: usize) -> Result<Detected, NotSerial> {
    if let Some(r) = detect_vendor(info, port) {
        return r;
    }
    if let Some(r) = detect_cdc(info, port) {
        return r;
    }
    if info.raw_descriptors.is_none() && !info.has_permission {
        return Err(NotSerial::NoPermission);
    }
    Err(NotSerial::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hubbridge_proto::fixtures as fx;

    #[test]
    fn vendor_chips() {
        let d = detect(&fx::cp2102_uart(1), 0).unwrap();
        assert_eq!(d.kind, DriverKind::Cp210x);
        assert_eq!((d.control_interface, d.claim.clone()), (0, vec![0]));
        assert_eq!((d.bulk_in, d.bulk_out, d.max_packet_in), (0x81, 0x01, 64));
        assert_eq!(d.label(), "CP210x");

        let d = detect(&fx::ft232r_uart(1), 0).unwrap();
        assert_eq!(
            (d.kind, d.chip.as_deref(), d.max_packet_in),
            (DriverKind::Ftdi, Some("FT232R"), 64)
        );
        assert_eq!(d.label(), "FTDI (FT232R)");

        let d = detect(&fx::ft232h_uart(1), 0).unwrap();
        assert_eq!((d.chip.as_deref(), d.max_packet_in), (Some("FT232H"), 512));

        let d = detect(&fx::ch340_uart(1), 0).unwrap();
        assert_eq!(d.kind, DriverKind::Ch34x);
        assert_eq!((d.bulk_in, d.bulk_out, d.max_packet_in), (0x82, 0x02, 32));
        assert_eq!(d.interrupt_in, Some((0x81, 8)));
    }

    #[test]
    fn multi_port_ftdi_picks_by_port() {
        let info = fx::ft2232h_uart(1);
        let a = detect(&info, 0).unwrap();
        let b = detect(&info, 1).unwrap();
        assert_eq!(
            (a.control_interface, a.bulk_in, a.bulk_out, a.ports),
            (0, 0x81, 0x02, 2)
        );
        assert_eq!((b.control_interface, b.bulk_in, b.bulk_out), (1, 0x83, 0x04));
        assert_eq!(detect(&info, 2), Err(NotSerial::NoSuchPort { port: 2, ports: 2 }));
    }

    #[test]
    fn cdc_with_union_on_a_composite_device() {
        let d = detect(&fx::cdc_acm_composite(1), 0).unwrap();
        assert_eq!(d.kind, DriverKind::CdcAcm);
        assert_eq!((d.control_interface, d.data_interface), (2, 3));
        assert_eq!(d.claim, vec![2, 3]);
        assert_eq!(d.interrupt_in, Some((0x83, 8)));
        assert_eq!((d.bulk_in, d.bulk_out), (0x81, 0x02));
        assert_eq!(d.acm_capabilities, 0x06);
    }

    #[test]
    fn cdc_simple_and_without_union() {
        let d = detect(&fx::cdc_acm_simple(1), 0).unwrap();
        assert_eq!(
            (d.control_interface, d.data_interface, d.acm_capabilities),
            (0, 1, 0x06)
        );
        // No functional descriptors at all: paired with the next data interface.
        let d = detect(&fx::cdc_acm_no_union(1), 0).unwrap();
        assert_eq!((d.control_interface, d.data_interface), (0, 1));
        assert_eq!(d.acm_capabilities, 0);
        assert_eq!(d.interrupt_in, Some((0x83, 8)));
    }

    #[test]
    fn class_based_fallback_without_raw_descriptors() {
        let mut info = fx::cdc_acm_composite(1);
        info.raw_descriptors = None;
        let d = detect(&info, 0).unwrap();
        // The next data interface after control 2 is 3.
        assert_eq!((d.control_interface, d.data_interface), (2, 3));
    }

    #[test]
    fn data_interface_alternate_with_endpoints() {
        // Data interface 1: alt 0 without endpoints, alt 1 with the bulk pair.
        let mut info = fx::cdc_acm_no_union(1);
        let cfg = &mut info.configurations[0];
        let mut alt0 = cfg.interfaces[1].clone();
        alt0.endpoints.clear();
        cfg.interfaces[1].alternate = 1;
        cfg.interfaces.insert(1, alt0);
        let d = detect(&info, 0).unwrap();
        assert_eq!((d.data_interface, d.data_alternate), (1, 1));
    }

    #[test]
    fn not_a_serial_device() {
        let e = detect(&fx::flash_drive(1), 0).unwrap_err();
        assert_eq!(e, NotSerial::Unsupported);
        assert!(e.to_string().contains("CDC-ACM, CP210x, FTDI, CH34x"));
        assert_eq!(detect(&fx::hid_keyboard(1), 0), Err(NotSerial::Unsupported));
    }

    #[test]
    fn descriptors_unknown_without_permission() {
        let e = detect(&fx::without_permission(fx::hid_keyboard(1)), 0).unwrap_err();
        assert_eq!(e, NotSerial::NoPermission);
        assert!(e.to_string().contains("hubbridge grant"));
    }

    #[test]
    fn old_ftdi_is_refused_with_a_reason() {
        let mut info = fx::ft232r_uart(1);
        info.bcd_device = Some(0x0200);
        match detect(&info, 0) {
            Err(NotSerial::Other(m)) => assert!(m.contains("0x0200")),
            other => panic!("{other:?}"),
        }
    }
}
