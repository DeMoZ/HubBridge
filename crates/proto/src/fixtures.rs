//! Realistic fake USB devices for the simulator and tests.

use crate::usb::UsbDeviceInfo;

/// Silicon Labs CP2102 USB-UART bridge: vendor-specific interface, bulk IN/OUT.
pub fn cp2102_raw() -> Vec<u8> {
    let mut v = vec![
        // device
        18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 64, 0xc4, 0x10, 0x60, 0xea, 0x00, 0x01, 1, 2, 3, 1,
    ];
    v.extend_from_slice(&[
        // config, wTotalLength = 32
        9, 0x02, 32, 0, 1, 1, 0, 0x80, 50, //
        // interface 0
        9, 0x04, 0, 0, 2, 0xff, 0x00, 0x00, 2, //
        // bulk IN 0x81, 64 bytes
        7, 0x05, 0x81, 0x02, 64, 0, 0, //
        // bulk OUT 0x01, 64 bytes
        7, 0x05, 0x01, 0x02, 64, 0, 0,
    ]);
    v
}

/// Boot keyboard: HID interface with a HID class descriptor and an interrupt IN endpoint.
pub fn hid_keyboard_raw() -> Vec<u8> {
    let mut v = vec![
        18, 0x01, 0x10, 0x01, 0x00, 0x00, 0x00, 8, 0x6d, 0x04, 0x1c, 0xc3, 0x10, 0x01, 1, 2, 0, 1,
    ];
    v.extend_from_slice(&[
        // config, wTotalLength = 34
        9, 0x02, 34, 0, 1, 1, 0, 0xa0, 50, //
        // interface 0: HID boot keyboard
        9, 0x04, 0, 0, 1, 0x03, 0x01, 0x01, 0, //
        // HID descriptor (class-specific, skipped by the parser)
        9, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 63, 0, //
        // interrupt IN 0x81, 8 bytes, every 10 ms
        7, 0x05, 0x81, 0x03, 8, 0, 10,
    ]);
    v
}

/// USB flash drive: mass storage, SCSI transparent command set, Bulk-Only Transport.
pub fn flash_drive_raw() -> Vec<u8> {
    let mut v = vec![
        18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 64, 0xcd, 0x14, 0x12, 0x12, 0x00, 0x01, 1, 2, 3, 1,
    ];
    v.extend_from_slice(&[
        // config, wTotalLength = 32
        9, 0x02, 32, 0, 1, 1, 0, 0x80, 50, //
        // interface 0: mass storage 08/06/50
        9, 0x04, 0, 0, 2, 0x08, 0x06, 0x50, 0, //
        // bulk IN 0x81, 512 bytes
        7, 0x05, 0x81, 0x02, 0x00, 0x02, 0, //
        // bulk OUT 0x02, 512 bytes
        7, 0x05, 0x02, 0x02, 0x00, 0x02, 0,
    ]);
    v
}

fn from_raw(id: u32, raw: Vec<u8>, manufacturer: &str, product: &str, serial: &str) -> UsbDeviceInfo {
    let mut d = UsbDeviceInfo {
        id,
        path: format!("/dev/bus/usb/001/{:03}", id % 1000),
        vendor_id: 0,
        product_id: 0,
        class: 0,
        subclass: 0,
        protocol: 0,
        bcd_usb: None,
        bcd_device: None,
        manufacturer: Some(manufacturer.into()),
        product: Some(product.into()),
        serial: Some(serial.into()),
        configurations: Vec::new(),
        has_permission: true,
        raw_descriptors: Some(raw),
    };
    d.apply_raw_descriptors().expect("fixture descriptors are valid");
    d
}

pub fn cp2102_uart(id: u32) -> UsbDeviceInfo {
    from_raw(
        id,
        cp2102_raw(),
        "Silicon Labs",
        "CP2102 USB to UART Bridge Controller",
        "0001",
    )
}

pub fn hid_keyboard(id: u32) -> UsbDeviceInfo {
    from_raw(id, hid_keyboard_raw(), "Logitech", "USB Keyboard", "KBD-42")
}

pub fn flash_drive(id: u32) -> UsbDeviceInfo {
    from_raw(id, flash_drive_raw(), "Generic", "Mass Storage Device", "121220160204")
}

/// A device the user has not granted permission for yet: Android shows only what
/// `UsbDevice` exposes, no serial and no raw descriptors.
pub fn without_permission(mut d: UsbDeviceInfo) -> UsbDeviceInfo {
    d.has_permission = false;
    d.serial = None;
    d.raw_descriptors = None;
    d.bcd_usb = None;
    d.bcd_device = None;
    d
}
