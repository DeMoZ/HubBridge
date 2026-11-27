//! Parser for raw USB descriptors.
//!
//! Input is what Android `UsbDeviceConnection.getRawDescriptors()` (and a read of the
//! Linux usbfs node) returns: the 18-byte device descriptor followed by every full
//! configuration descriptor (config + interface + endpoint + class-specific descriptors).

use thiserror::Error;

use crate::usb::{ConfigInfo, EndpointInfo, InterfaceInfo};

pub const DT_DEVICE: u8 = 0x01;
pub const DT_CONFIG: u8 = 0x02;
pub const DT_INTERFACE: u8 = 0x04;
pub const DT_ENDPOINT: u8 = 0x05;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DescriptorError {
    #[error("descriptor data too short")]
    TooShort,
    #[error("first descriptor is not a device descriptor")]
    NoDeviceDescriptor,
    #[error("descriptor at offset {0} has an invalid length")]
    BadLength(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub bcd_usb: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub max_packet_size0: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub i_manufacturer: u8,
    pub i_product: u8,
    pub i_serial: u8,
    pub num_configurations: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub device: DeviceDescriptor,
    pub configs: Vec<ConfigInfo>,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

/// Parse a device descriptor followed by any number of configuration descriptors.
/// Unknown and class-specific descriptors are skipped.
pub fn parse(raw: &[u8]) -> Result<Parsed, DescriptorError> {
    if raw.len() < 18 {
        return Err(DescriptorError::TooShort);
    }
    if raw[0] < 18 || raw[1] != DT_DEVICE {
        return Err(DescriptorError::NoDeviceDescriptor);
    }
    let device = DeviceDescriptor {
        bcd_usb: le16(raw, 2),
        class: raw[4],
        subclass: raw[5],
        protocol: raw[6],
        max_packet_size0: raw[7],
        vendor_id: le16(raw, 8),
        product_id: le16(raw, 10),
        bcd_device: le16(raw, 12),
        i_manufacturer: raw[14],
        i_product: raw[15],
        i_serial: raw[16],
        num_configurations: raw[17],
    };

    let mut configs: Vec<ConfigInfo> = Vec::new();
    let mut pos = raw[0] as usize;
    while pos < raw.len() {
        if pos + 2 > raw.len() {
            return Err(DescriptorError::TooShort);
        }
        let len = raw[pos] as usize;
        if len < 2 || pos + len > raw.len() {
            return Err(DescriptorError::BadLength(pos));
        }
        let d = &raw[pos..pos + len];
        match d[1] {
            DT_CONFIG if len >= 9 => configs.push(ConfigInfo {
                value: d[5],
                name: None,
                attributes: d[7],
                max_power_ma: d[8] as u16 * 2,
                interfaces: Vec::new(),
            }),
            DT_INTERFACE if len >= 9 => {
                if let Some(cfg) = configs.last_mut() {
                    cfg.interfaces.push(InterfaceInfo {
                        number: d[2],
                        alternate: d[3],
                        class: d[5],
                        subclass: d[6],
                        protocol: d[7],
                        name: None,
                        endpoints: Vec::new(),
                    });
                }
            }
            DT_ENDPOINT if len >= 7 => {
                if let Some(intf) = configs.last_mut().and_then(|c| c.interfaces.last_mut()) {
                    intf.endpoints.push(EndpointInfo {
                        address: d[2],
                        attributes: d[3],
                        max_packet_size: le16(d, 4) & 0x07ff,
                        interval: d[6],
                    });
                }
            }
            _ => {}
        }
        pos += len;
    }
    Ok(Parsed { device, configs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::usb::TransferType;

    #[test]
    fn parses_cp2102() {
        let p = parse(&fixtures::cp2102_raw()).unwrap();
        assert_eq!(p.device.vendor_id, 0x10c4);
        assert_eq!(p.device.product_id, 0xea60);
        assert_eq!(p.configs.len(), 1);
        let intf = &p.configs[0].interfaces[0];
        assert_eq!(intf.class, 0xff);
        assert_eq!(intf.endpoints.len(), 2);
        assert!(intf.endpoints.iter().all(|e| e.transfer_type() == TransferType::Bulk));
        assert_eq!(p.configs[0].max_power_ma, 100);
    }

    #[test]
    fn parses_hid_and_skips_class_descriptor() {
        let p = parse(&fixtures::hid_keyboard_raw()).unwrap();
        let intf = &p.configs[0].interfaces[0];
        assert_eq!((intf.class, intf.subclass, intf.protocol), (3, 1, 1));
        assert_eq!(intf.endpoints.len(), 1);
        let ep = &intf.endpoints[0];
        assert!(ep.is_in());
        assert_eq!(ep.transfer_type(), TransferType::Interrupt);
        assert_eq!(ep.max_packet_size, 8);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse(&[1, 2, 3]), Err(DescriptorError::TooShort));
        let mut raw = fixtures::cp2102_raw();
        raw[1] = 0x02;
        assert_eq!(parse(&raw), Err(DescriptorError::NoDeviceDescriptor));
        let mut raw = fixtures::cp2102_raw();
        let n = raw.len();
        raw[18] = 200; // config length past the end
        assert!(matches!(parse(&raw[..n]), Err(DescriptorError::BadLength(18))));
    }
}
