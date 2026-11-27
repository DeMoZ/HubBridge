//! USB device model shared by the agent and the host.

use serde::{Deserialize, Serialize};

use crate::descriptors::{self, DescriptorError};

/// One USB device attached to the phone (directly or behind the hub).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDeviceInfo {
    /// Agent-assigned id, stable while the device stays plugged in
    /// (Android `UsbDevice.getDeviceId()`).
    pub id: u32,
    /// OS path on the phone, e.g. `/dev/bus/usb/001/004`.
    pub path: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// `bcdUSB` (e.g. 0x0200). Known only once raw descriptors were read.
    pub bcd_usb: Option<u16>,
    /// `bcdDevice`.
    pub bcd_device: Option<u16>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    /// Android returns the serial only after the user granted USB permission.
    pub serial: Option<String>,
    pub configurations: Vec<ConfigInfo>,
    /// The user granted this app access to the device (`UsbManager.hasPermission`).
    pub has_permission: bool,
    /// Device descriptor followed by all configuration descriptors, exactly as the OS
    /// returns them. Needs permission on Android.
    pub raw_descriptors: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigInfo {
    /// `bConfigurationValue`.
    pub value: u8,
    pub name: Option<String>,
    /// `bmAttributes` (bit 6 self-powered, bit 5 remote wakeup).
    pub attributes: u8,
    pub max_power_ma: u16,
    /// Every interface descriptor, including alternate settings.
    pub interfaces: Vec<InterfaceInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceInfo {
    pub number: u8,
    pub alternate: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub name: Option<String>,
    pub endpoints: Vec<EndpointInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointInfo {
    /// `bEndpointAddress`: bit 7 is the direction (1 = IN), bits 0..3 the number.
    pub address: u8,
    /// `bmAttributes`: bits 0..1 are the transfer type.
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

impl EndpointInfo {
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }

    pub fn transfer_type(&self) -> TransferType {
        match self.attributes & 0x03 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }
}

impl UsbDeviceInfo {
    /// Fill `bcd_usb`, `bcd_device` and `configurations` from the raw descriptors, if present
    /// and valid. Keeps the existing values when parsing fails.
    pub fn apply_raw_descriptors(&mut self) -> Result<(), DescriptorError> {
        let Some(raw) = &self.raw_descriptors else {
            return Ok(());
        };
        let parsed = descriptors::parse(raw)?;
        self.bcd_usb = Some(parsed.device.bcd_usb);
        self.bcd_device = Some(parsed.device.bcd_device);
        self.vendor_id = parsed.device.vendor_id;
        self.product_id = parsed.device.product_id;
        self.class = parsed.device.class;
        self.subclass = parsed.device.subclass;
        self.protocol = parsed.device.protocol;
        if !parsed.configs.is_empty() {
            // Raw descriptors carry no strings: keep names Android resolved for us.
            let mut configs = parsed.configs;
            for cfg in &mut configs {
                if let Some(old) = self.configurations.iter().find(|c| c.value == cfg.value) {
                    cfg.name = old.name.clone();
                    for intf in &mut cfg.interfaces {
                        if let Some(o) = old
                            .interfaces
                            .iter()
                            .find(|i| i.number == intf.number && i.alternate == intf.alternate)
                        {
                            intf.name = o.name.clone();
                        }
                    }
                }
            }
            self.configurations = configs;
        }
        Ok(())
    }

    /// Short "vvvv:pppp Product" label for lists.
    pub fn label(&self) -> String {
        let name = self.product.as_deref().unwrap_or("(unnamed)");
        format!("{:04x}:{:04x} {}", self.vendor_id, self.product_id, name)
    }
}

/// Human-readable name of a USB class code (device or interface).
pub fn class_name(class: u8) -> &'static str {
    match class {
        0x00 => "per-interface",
        0x01 => "Audio",
        0x02 => "CDC (comm)",
        0x03 => "HID",
        0x05 => "Physical",
        0x06 => "Image",
        0x07 => "Printer",
        0x08 => "Mass Storage",
        0x09 => "Hub",
        0x0a => "CDC Data",
        0x0b => "Smart Card",
        0x0d => "Content Security",
        0x0e => "Video",
        0x0f => "Personal Healthcare",
        0x10 => "Audio/Video",
        0x11 => "Billboard",
        0x12 => "USB-C Bridge",
        0xdc => "Diagnostic",
        0xe0 => "Wireless",
        0xef => "Miscellaneous",
        0xfe => "Application Specific",
        0xff => "Vendor Specific",
        _ => "Unknown",
    }
}
