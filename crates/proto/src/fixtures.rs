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

// --- serial adapters and CDC-ACM devices (step 2b) ---------------------------------------

/// Device descriptor of the serial fixtures.
fn device_desc(vid: u16, pid: u16, bcd_device: u16, class: (u8, u8, u8), max_packet0: u8) -> Vec<u8> {
    let mut v = vec![18, 0x01, 0x00, 0x02, class.0, class.1, class.2, max_packet0];
    v.extend_from_slice(&vid.to_le_bytes());
    v.extend_from_slice(&pid.to_le_bytes());
    v.extend_from_slice(&bcd_device.to_le_bytes());
    v.extend_from_slice(&[1, 2, 3, 1]);
    v
}

/// Device descriptor + configuration descriptor (wTotalLength computed) around `body`.
fn with_config(device: Vec<u8>, num_interfaces: u8, body: Vec<u8>) -> Vec<u8> {
    let total = 9 + body.len();
    let mut v = device;
    v.extend_from_slice(&[9, 0x02, total as u8, (total >> 8) as u8, num_interfaces, 1, 0, 0x80, 50]);
    v.extend(body);
    v
}

fn interface_desc(number: u8, endpoints: u8, class: (u8, u8, u8)) -> Vec<u8> {
    vec![9, 0x04, number, 0, endpoints, class.0, class.1, class.2, 0]
}

fn endpoint_desc(address: u8, attributes: u8, max_packet: u16, interval: u8) -> Vec<u8> {
    vec![
        7,
        0x05,
        address,
        attributes,
        max_packet as u8,
        (max_packet >> 8) as u8,
        interval,
    ]
}

fn bulk_pair(intf: u8, in_ep: u8, out_ep: u8, max_packet: u16, class: (u8, u8, u8)) -> Vec<u8> {
    let mut v = interface_desc(intf, 2, class);
    v.extend(endpoint_desc(in_ep, 0x02, max_packet, 0));
    v.extend(endpoint_desc(out_ep, 0x02, max_packet, 0));
    v
}

/// FTDI FT232R (0403:6001, bcdDevice 0x0600): one vendor interface, bulk 0x81 / 0x02, 64 bytes.
pub fn ft232r_raw() -> Vec<u8> {
    with_config(
        device_desc(0x0403, 0x6001, 0x0600, (0, 0, 0), 8),
        1,
        bulk_pair(0, 0x81, 0x02, 64, (0xff, 0xff, 0xff)),
    )
}

/// FTDI FT2232H (0403:6010, bcdDevice 0x0700): two vendor interfaces (ports A and B), high
/// speed, bulk 0x81 / 0x02 and 0x83 / 0x04, 512 bytes.
pub fn ft2232h_raw() -> Vec<u8> {
    let mut body = bulk_pair(0, 0x81, 0x02, 512, (0xff, 0xff, 0xff));
    body.extend(bulk_pair(1, 0x83, 0x04, 512, (0xff, 0xff, 0xff)));
    with_config(device_desc(0x0403, 0x6010, 0x0700, (0, 0, 0), 64), 2, body)
}

/// FTDI FT232H (0403:6014, bcdDevice 0x0900): high speed, bulk 0x81 / 0x02, 512 bytes.
pub fn ft232h_raw() -> Vec<u8> {
    with_config(
        device_desc(0x0403, 0x6014, 0x0900, (0, 0, 0), 64),
        1,
        bulk_pair(0, 0x81, 0x02, 512, (0xff, 0xff, 0xff)),
    )
}

/// WCH CH340 (1a86:7523): vendor interface ff/01/02, bulk IN 0x82 and OUT 0x02 (32 bytes),
/// interrupt IN 0x81 (8 bytes) for the modem status.
pub fn ch340_raw() -> Vec<u8> {
    let mut body = interface_desc(0, 3, (0xff, 0x01, 0x02));
    body.extend(endpoint_desc(0x82, 0x02, 32, 0));
    body.extend(endpoint_desc(0x02, 0x02, 32, 0));
    body.extend(endpoint_desc(0x81, 0x03, 8, 1));
    with_config(device_desc(0x1a86, 0x7523, 0x0254, (0xff, 0, 0), 8), 1, body)
}

/// CDC functional descriptors of an ACM function: header, call management (data interface
/// `data`), ACM capabilities 0x06 (line coding + break), and a union when `union` is set.
fn cdc_functional(control: u8, data: u8, union: bool) -> Vec<u8> {
    let mut v = vec![5, 0x24, 0x00, 0x10, 0x01]; // header, CDC 1.10
    v.extend_from_slice(&[5, 0x24, 0x01, 0x00, data]); // call management
    v.extend_from_slice(&[4, 0x24, 0x02, 0x06]); // ACM: line coding + send break
    if union {
        v.extend_from_slice(&[5, 0x24, 0x06, control, data]);
    }
    v
}

/// Interface association descriptor: `count` interfaces from `first`, class CDC / ACM.
fn iad(first: u8, count: u8) -> Vec<u8> {
    vec![8, 0x0b, first, count, 0x02, 0x02, 0x00, 0]
}

/// CDC-ACM device 1209:0001 with an IAD, control interface 0 (interrupt IN 0x83, header,
/// call management, ACM and union descriptors) and data interface 1 (bulk 0x81 / 0x02, 64).
pub fn cdc_acm_simple_raw() -> Vec<u8> {
    let mut body = iad(0, 2);
    body.extend(interface_desc(0, 1, (0x02, 0x02, 0x00)));
    body.extend(cdc_functional(0, 1, true));
    body.extend(endpoint_desc(0x83, 0x03, 8, 16));
    body.extend(bulk_pair(1, 0x81, 0x02, 64, (0x0a, 0x00, 0x00)));
    with_config(device_desc(0x1209, 0x0001, 0x0100, (0xef, 0x02, 0x01), 64), 2, body)
}

/// Composite device: two vendor interfaces (0 with a bulk pair 0x84 / 0x04, 1 without
/// endpoints), then an IAD at 2, CDC control interface 2 (interrupt IN 0x83, union naming
/// data interface 3) and data interface 3 (bulk 0x81 / 0x02, 64).
pub fn cdc_acm_composite_raw() -> Vec<u8> {
    let mut body = bulk_pair(0, 0x84, 0x04, 64, (0xff, 0x00, 0x00));
    body.extend(interface_desc(1, 0, (0xff, 0x00, 0x00)));
    body.extend(iad(2, 2));
    body.extend(interface_desc(2, 1, (0x02, 0x02, 0x01)));
    body.extend(cdc_functional(2, 3, true));
    body.extend(endpoint_desc(0x83, 0x03, 8, 16));
    body.extend(bulk_pair(3, 0x81, 0x02, 64, (0x0a, 0x00, 0x00)));
    with_config(device_desc(0x1209, 0x0002, 0x0100, (0xef, 0x02, 0x01), 64), 4, body)
}

/// CDC-ACM device without functional descriptors (Chameleon Ultra layout): control interface
/// 0 (class 02/02/01, interrupt IN 0x83) and data interface 1 (bulk 0x81 / 0x02, 64).
pub fn cdc_acm_no_union_raw() -> Vec<u8> {
    let mut body = interface_desc(0, 1, (0x02, 0x02, 0x01));
    body.extend(endpoint_desc(0x83, 0x03, 8, 16));
    body.extend(bulk_pair(1, 0x81, 0x02, 64, (0x0a, 0x00, 0x00)));
    with_config(device_desc(0x6868, 0x8686, 0x0100, (0x02, 0, 0), 64), 2, body)
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

pub fn ft232r_uart(id: u32) -> UsbDeviceInfo {
    from_raw(id, ft232r_raw(), "FTDI", "FT232R USB UART", "A50285BI")
}

pub fn ft2232h_uart(id: u32) -> UsbDeviceInfo {
    from_raw(id, ft2232h_raw(), "FTDI", "Dual RS232-HS", "FT2232H01")
}

pub fn ft232h_uart(id: u32) -> UsbDeviceInfo {
    from_raw(id, ft232h_raw(), "FTDI", "Single RS232-HS", "FT232H001")
}

pub fn ch340_uart(id: u32) -> UsbDeviceInfo {
    from_raw(id, ch340_raw(), "wch.cn", "USB Serial", "0001")
}

pub fn cdc_acm_simple(id: u32) -> UsbDeviceInfo {
    from_raw(id, cdc_acm_simple_raw(), "HubBridge", "CDC ACM Serial", "CDC0001")
}

pub fn cdc_acm_composite(id: u32) -> UsbDeviceInfo {
    from_raw(id, cdc_acm_composite_raw(), "HubBridge", "Composite CDC", "CDC0002")
}

pub fn cdc_acm_no_union(id: u32) -> UsbDeviceInfo {
    from_raw(id, cdc_acm_no_union_raw(), "Proxgrind", "ChameleonUltra", "CU0001")
}
