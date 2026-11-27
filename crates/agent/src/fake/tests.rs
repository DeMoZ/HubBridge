//! Tests of the fake backend itself: generic transfer rules and the serial chips.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hubbridge_proto::{Setup, UsbStatus, fixtures};

use super::*;
use crate::usb::UsbBackend;

const T: Duration = Duration::from_secs(2);

async fn open(usb: &FakeUsb, id: u32, claim: &[u8]) -> Arc<dyn UsbDevice> {
    let dev = usb.open(id).await.unwrap();
    for i in claim {
        dev.claim(*i, false).await.unwrap();
    }
    dev
}

fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Setup {
    Setup {
        request_type,
        request,
        value,
        index,
        length,
    }
}

async fn ctl_out(dev: &Arc<dyn UsbDevice>, s: Setup, data: &[u8]) -> TransferResult {
    dev.control(s, data.to_vec(), T).await
}

async fn ctl_in(dev: &Arc<dyn UsbDevice>, s: Setup) -> TransferResult {
    dev.control(s, Vec::new(), T).await
}

fn serial_fake(id: u32, raw: Vec<u8>, chip: SerialChip) -> Arc<FakeUsb> {
    let usb = FakeUsb::new();
    usb.add(id, raw, &["Maker", "Serial", "1"], FakeKind::Serial(chip), true);
    usb
}

#[tokio::test]
async fn bulk_checks_the_claim_of_the_owning_interface() {
    let usb = serial_fake(1, fixtures::ft2232h_raw(), SerialChip::Ftdi);
    let dev = open(&usb, 1, &[0]).await;
    // Port B's endpoints belong to interface 1.
    let r = dev.transfer(0x04, 0, b"x".to_vec(), T).await;
    assert_eq!(r.status, UsbStatus::InvalidArgument);
    // Port A's OUT works on interface 0.
    assert_eq!(dev.transfer(0x02, 0, b"x".to_vec(), T).await.status, UsbStatus::Ok);
    dev.claim(1, false).await.unwrap();
    assert_eq!(dev.transfer(0x04, 0, b"x".to_vec(), T).await.status, UsbStatus::Ok);
    // An endpoint the device does not have.
    assert_eq!(
        dev.transfer(0x09, 0, b"x".to_vec(), T).await.status,
        UsbStatus::InvalidArgument
    );
}

fn loopback() -> Arc<FakeUsb> {
    let usb = FakeUsb::new();
    usb.add(1, fixtures::cp2102_raw(), &[], FakeKind::Loopback, true);
    usb
}

#[tokio::test]
async fn waiting_in_transfers_are_served_in_submission_order() {
    let usb = loopback();
    let dev = open(&usb, 1, &[0]).await;
    let ins: Vec<_> = (0..3).map(|_| dev.transfer(0x81, 1, vec![], T)).collect();
    for b in [10u8, 20, 30] {
        assert_eq!(dev.transfer(0x01, 0, vec![b], T).await.status, UsbStatus::Ok);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut got = Vec::new();
    for f in ins {
        let r = f.await;
        assert_eq!(r.status, UsbStatus::Ok);
        got.extend(r.data);
    }
    assert_eq!(got, vec![10, 20, 30]);
}

#[tokio::test]
async fn front_timeout_cancels_every_waiting_transfer() {
    let usb = loopback();
    let dev = open(&usb, 1, &[0]).await;
    let a = dev.transfer(0x81, 1, vec![], Duration::from_millis(100));
    let b = dev.transfer(0x81, 1, vec![], Duration::from_secs(30));
    let c = dev.transfer(0x81, 1, vec![], Duration::from_secs(30));
    let start = Instant::now();
    for f in [a, b, c] {
        assert_eq!(f.await.status, UsbStatus::Cancelled);
    }
    assert!(start.elapsed() < Duration::from_secs(2));
    // The endpoint works again afterwards.
    dev.transfer(0x01, 0, vec![7], T).await;
    assert_eq!(dev.transfer(0x81, 1, vec![], T).await.data, vec![7]);
}

#[tokio::test]
async fn cp210x_requests_are_decoded_and_data_waits_for_ifc_enable() {
    let usb = serial_fake(5, fixtures::cp2102_raw(), SerialChip::Cp210x { partnum: 0x20 });
    let dev = open(&usb, 5, &[0]).await;
    // Before IFC_ENABLE the chip discards OUT data.
    assert_eq!(dev.transfer(0x01, 0, b"lost".to_vec(), T).await.status, UsbStatus::Ok);
    let r = dev.transfer(0x81, 64, vec![], Duration::from_millis(60)).await;
    assert_eq!((r.status, r.data.len()), (UsbStatus::Cancelled, 0));

    assert_eq!(
        ctl_in(&dev, setup(0xc0, 0xff, 0x370b, 0, 1)).await.data,
        vec![0x20],
        "part number"
    );
    for (s, data) in [
        (setup(0x41, 0x00, 1, 0, 0), vec![]),
        (setup(0x41, 0x1e, 0, 0, 4), vec![0x00, 0xc2, 0x01, 0x00]),
        (setup(0x41, 0x03, 0x0722, 0, 0), vec![]),
        (setup(0x41, 0x07, 0x0303, 0, 0), vec![]),
        (setup(0x41, 0x05, 1, 0, 0), vec![]),
        (setup(0x41, 0x12, 0x000f, 0, 0), vec![]),
    ] {
        assert_eq!(ctl_out(&dev, s, &data).await.status, UsbStatus::Ok);
    }
    let d = usb.serial_state(5);
    assert_eq!((d.baud, d.data_bits, d.parity, d.stop_bits), (115200, 7, 2, 2));
    assert!(d.enabled && d.dtr && d.rts && d.break_on);
    assert_eq!((d.purged_rx, d.purged_tx), (1, 1));

    // Flow control block round trip.
    let flow = ctl_in(&dev, setup(0xc1, 0x14, 0, 0, 16)).await.data;
    assert_eq!(flow.len(), 16);
    let mut new = flow.clone();
    new[0] |= 0x08;
    new[4] = (new[4] & 0x3f) | 0x80;
    assert_eq!(
        ctl_out(&dev, setup(0x41, 0x13, 0, 0, 16), &new).await.status,
        UsbStatus::Ok
    );
    assert!(usb.serial_state(5).flow_rtscts);

    // Modem status and comm status.
    usb.set_modem(
        5,
        ModemLines {
            cts: true,
            dcd: true,
            ..Default::default()
        },
    );
    assert_eq!(ctl_in(&dev, setup(0xc1, 0x08, 0, 0, 1)).await.data, vec![0x93]);
    usb.inject_line_error(5, LineErrorKind::Overrun);
    let st = ctl_in(&dev, setup(0xc1, 0x10, 0, 0, 19)).await.data;
    assert_eq!((st.len(), st[0]), (19, 0x04));

    // Enabled: data loops back. Unknown requests stall.
    dev.transfer(0x01, 0, b"hi".to_vec(), T).await;
    assert_eq!(dev.transfer(0x81, 64, vec![], T).await.data, b"hi");
    assert_eq!(
        ctl_out(&dev, setup(0x41, 0x77, 0, 0, 0), &[]).await.status,
        UsbStatus::Stall
    );

    // The log has everything in order, requests and writes.
    let log = usb.serial_log(5);
    assert!(matches!(log[0], SerialEvent::Out { len: 4 }));
    assert!(matches!(&log[1], SerialEvent::Control { setup, .. } if setup.request == 0xff));
    assert!(matches!(log.last(), Some(SerialEvent::Control { setup, .. }) if setup.request == 0x77));
}

#[tokio::test]
async fn ftdi_in_packets_carry_status_headers() {
    let usb = serial_fake(6, fixtures::ft232r_raw(), SerialChip::Ftdi);
    let dev = open(&usb, 6, &[0]).await;
    // Port byte is checked: FT232R uses channel 0.
    assert_eq!(
        ctl_out(&dev, setup(0x40, 0x03, 0x4138, 1, 0), &[]).await.status,
        UsbStatus::Stall
    );
    assert_eq!(
        ctl_out(&dev, setup(0x40, 0x03, 0x001a, 0, 0), &[]).await.status,
        UsbStatus::Ok
    );
    let d = usb.serial_state(6);
    assert!((115_000..=115_500).contains(&d.baud), "{}", d.baud);
    ctl_out(&dev, setup(0x40, 0x04, 0x0207, 0, 0), &[]).await;
    let d = usb.serial_state(6);
    assert_eq!((d.data_bits, d.parity, d.stop_bits), (7, 2, 0));
    ctl_out(&dev, setup(0x40, 0x01, 0x0100, 0, 0), &[]).await;
    assert!(!usb.serial_state(6).dtr);

    // 200 bytes written: four packets of up to 64 bytes, each with 2 header bytes.
    let payload: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
    dev.transfer(0x02, 0, payload.clone(), T).await;
    let r = dev.transfer(0x81, 256, vec![], T).await;
    assert_eq!(r.data.len(), 200 + 8);
    let mut data = Vec::new();
    for chunk in r.data.chunks(64) {
        assert_eq!(chunk[1] & 0x60, 0x60, "header byte 1");
        data.extend_from_slice(&chunk[2..]);
    }
    assert_eq!(data, payload);

    // Idle: a status-only packet after the latency timer (16 ms by default).
    let start = Instant::now();
    let r = dev.transfer(0x81, 64, vec![], T).await;
    assert_eq!((r.status, r.data.len()), (UsbStatus::Ok, 2));
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(10) && waited < Duration::from_millis(500),
        "{waited:?}"
    );

    // Modem lines show in the header, line errors on the next data packet.
    usb.set_modem(
        6,
        ModemLines {
            cts: true,
            ..Default::default()
        },
    );
    let r = dev.transfer(0x81, 64, vec![], T).await;
    assert_eq!(r.data, vec![0x10, 0x60]);
    usb.inject_line_error(6, LineErrorKind::Overrun);
    dev.transfer(0x02, 0, b"z".to_vec(), T).await;
    let r = dev.transfer(0x81, 64, vec![], T).await;
    assert_eq!(r.data, vec![0x10, 0x62, b'z']);
}

#[tokio::test]
async fn ftdi_second_port_uses_its_own_channel() {
    let usb = serial_fake(7, fixtures::ft2232h_raw(), SerialChip::Ftdi);
    let dev = open(&usb, 7, &[0, 1]).await;
    // Port B: channel 2; value 0xc068 index 0x0202 is 115200 on the 120 MHz base.
    assert_eq!(
        ctl_out(&dev, setup(0x40, 0x03, 0xc068, 0x0202, 0), &[]).await.status,
        UsbStatus::Ok
    );
    let d = usb.serial_state_port(7, 1);
    assert!((115_000..=115_500).contains(&d.baud), "{}", d.baud);
    assert_eq!(usb.serial_state_port(7, 0).baud, 9600);
    // Channel 3 does not exist.
    assert_eq!(
        ctl_out(&dev, setup(0x40, 0x03, 0xc068, 0x0203, 0), &[]).await.status,
        UsbStatus::Stall
    );
    dev.transfer(0x04, 0, b"B".to_vec(), T).await;
    assert_eq!(dev.transfer(0x83, 512, vec![], T).await.data[2..], *b"B");
}

#[tokio::test]
async fn ch34x_requests_and_interrupt_packets() {
    let chip = SerialChip::Ch34x {
        version: 0x31,
        limited_prescaler: false,
    };
    let usb = serial_fake(8, fixtures::ch340_raw(), chip);
    let dev = open(&usb, 8, &[0]).await;
    assert_eq!(ctl_in(&dev, setup(0xc0, 0x5f, 0, 0, 2)).await.data, vec![0x31, 0]);
    ctl_out(&dev, setup(0x40, 0xa1, 0, 0, 0), &[]).await;
    ctl_out(&dev, setup(0x40, 0x9a, 0x1312, 0xcc83, 0), &[]).await;
    let d = usb.serial_state(8);
    assert!((115_000..=115_500).contains(&d.baud), "{}", d.baud);
    ctl_out(&dev, setup(0x40, 0x9a, 0x2518, 0x00c3, 0), &[]).await;
    let d = usb.serial_state(8);
    assert_eq!((d.data_bits, d.parity, d.stop_bits), (8, 0, 0));
    ctl_out(&dev, setup(0x40, 0xa4, 0xff9f, 0, 0), &[]).await;
    let d = usb.serial_state(8);
    assert!(d.dtr && d.rts);
    ctl_out(&dev, setup(0x40, 0xa4, 0xffff, 0, 0), &[]).await;
    assert!(!usb.serial_state(8).dtr);
    // Modem status register is inverted.
    assert_eq!(ctl_in(&dev, setup(0xc0, 0x95, 0x0706, 0, 2)).await.data, vec![0xff, 0]);

    // Interrupt packet [0, 0, !status, 0] when the lines change (CTS + DCD = 0x09).
    let intr = dev.transfer(0x81, 8, vec![], T);
    usb.set_modem(
        8,
        ModemLines {
            cts: true,
            dcd: true,
            ..Default::default()
        },
    );
    assert_eq!(intr.await.data, vec![0, 0, !0x09u8, 0]);
    // Break register read-modify-write.
    let reg = ctl_in(&dev, setup(0xc0, 0x95, 0x1805, 0, 2)).await.data;
    assert_eq!(reg, vec![0x01, 0xc3]);
    ctl_out(&dev, setup(0x40, 0x9a, 0x1805, 0x8300, 0), &[]).await;
    assert!(usb.serial_state(8).break_on);

    // With the quirk the break register read stalls.
    let usb = serial_fake(
        9,
        fixtures::ch340_raw(),
        SerialChip::Ch34x {
            version: 0x27,
            limited_prescaler: true,
        },
    );
    let dev = open(&usb, 9, &[0]).await;
    assert_eq!(
        ctl_in(&dev, setup(0xc0, 0x95, 0x0005, 0, 2)).await.status,
        UsbStatus::Stall
    );
    assert_eq!(
        ctl_in(&dev, setup(0xc0, 0x95, 0x1805, 0, 2)).await.status,
        UsbStatus::Stall
    );
}

#[tokio::test]
async fn cdc_acm_line_coding_and_serial_state() {
    let usb = serial_fake(10, fixtures::cdc_acm_composite_raw(), SerialChip::CdcAcm);
    let dev = open(&usb, 10, &[2, 3]).await;
    // Control interface 2: requests for another interface stall.
    assert_eq!(
        ctl_out(&dev, setup(0x21, 0x22, 3, 0, 0), &[]).await.status,
        UsbStatus::Stall
    );
    let coding = [0x00, 0xc2, 0x01, 0x00, 0x00, 0x00, 0x08];
    assert_eq!(
        ctl_out(&dev, setup(0x21, 0x20, 0, 2, 7), &coding).await.status,
        UsbStatus::Ok
    );
    assert_eq!(ctl_in(&dev, setup(0xa1, 0x21, 0, 2, 7)).await.data, coding);
    ctl_out(&dev, setup(0x21, 0x22, 3, 2, 0), &[]).await;
    let d = usb.serial_state(10);
    assert_eq!(d.baud, 115200);
    assert!(d.dtr && d.rts);
    ctl_out(&dev, setup(0x21, 0x23, 0xffff, 2, 0), &[]).await;
    assert!(usb.serial_state(10).break_on);

    let intr = dev.transfer(0x83, 16, vec![], T);
    usb.set_modem(
        10,
        ModemLines {
            dcd: true,
            dsr: true,
            ..Default::default()
        },
    );
    assert_eq!(
        intr.await.data,
        vec![0xa1, 0x20, 0, 0, 2, 0, 2, 0, 0x03, 0x00],
        "SERIAL_STATE for control interface 2"
    );
    dev.transfer(0x02, 0, b"cdc".to_vec(), T).await;
    assert_eq!(dev.transfer(0x81, 64, vec![], T).await.data, b"cdc");
    usb.inject_rx(10, b"unsolicited");
    assert_eq!(dev.transfer(0x81, 64, vec![], T).await.data, b"unsolicited");
}

#[tokio::test]
async fn serial_state_survives_reopen_and_stall_hook() {
    let usb = serial_fake(11, fixtures::cp2102_raw(), SerialChip::Cp210x { partnum: 2 });
    let dev = open(&usb, 11, &[0]).await;
    ctl_out(&dev, setup(0x41, 0x1e, 0, 0, 4), &[0x00, 0xc2, 0x01, 0x00]).await;
    dev.close();
    let dev = open(&usb, 11, &[0]).await;
    assert_eq!(usb.serial_state(11).baud, 115200);
    ctl_out(&dev, setup(0x41, 0x00, 1, 0, 0), &[]).await;
    usb.stall_next_in(11);
    assert_eq!(dev.transfer(0x81, 8, vec![], T).await.status, UsbStatus::Stall);
    usb.inject_rx(11, b"ok");
    assert_eq!(dev.transfer(0x81, 8, vec![], T).await.data, b"ok");
}
