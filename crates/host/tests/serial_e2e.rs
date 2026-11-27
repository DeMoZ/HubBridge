//! Serial ports end to end over real TCP: host serial core ↔ agent core ↔ fake serial chips.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hubbridge_agent::fake::{FakeKind, FakeUsb, LineErrorKind, ModemLines, SerialChip, SerialEvent};
use hubbridge_agent::{Agent, AgentConfig};
use hubbridge_host::serial::{Flow, Line, Parity, PortEvent, PortOptions, SerialConfig, SerialPort, StopBits};
use hubbridge_host::{Client, Store};
use hubbridge_proto::{UsbDeviceInfo, fixtures};

const CP2102: u32 = 1;
const FT232R: u32 = 2;
const FT2232H: u32 = 3;
const FT232H: u32 = 4;
const CH340: u32 = 5;
const CDC: u32 = 6;
const CP2102N: u32 = 7;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hubbridge-serial-{tag}-{}", hubbridge_link::random_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Env {
    agent: Agent,
    usb: Arc<FakeUsb>,
    client: Arc<Client>,
    infos: Vec<UsbDeviceInfo>,
}

async fn env() -> Env {
    let agent = Agent::new(AgentConfig {
        state_dir: temp_dir("agent"),
        name: "Serial Phone".into(),
        model: "test".into(),
        os: "test".into(),
        app_version: "0".into(),
        usb_host: true,
    })
    .unwrap();
    let infos = vec![
        fixtures::cp2102_uart(CP2102),
        fixtures::ft232r_uart(FT232R),
        fixtures::ft2232h_uart(FT2232H),
        fixtures::ft232h_uart(FT232H),
        fixtures::ch340_uart(CH340),
        fixtures::cdc_acm_simple(CDC),
        fixtures::cp2102_uart(CP2102N),
        fixtures::flash_drive(8),
    ];
    agent.set_devices(infos.clone());
    let usb = FakeUsb::new();
    let add =
        |id: u32, raw: Vec<u8>, chip: SerialChip| usb.add(id, raw, &["M", "P", "S"], FakeKind::Serial(chip), true);
    add(CP2102, fixtures::cp2102_raw(), SerialChip::Cp210x { partnum: 2 });
    add(FT232R, fixtures::ft232r_raw(), SerialChip::Ftdi);
    add(FT2232H, fixtures::ft2232h_raw(), SerialChip::Ftdi);
    add(FT232H, fixtures::ft232h_raw(), SerialChip::Ftdi);
    add(
        CH340,
        fixtures::ch340_raw(),
        SerialChip::Ch34x {
            version: 0x31,
            limited_prescaler: false,
        },
    );
    add(CDC, fixtures::cdc_acm_simple_raw(), SerialChip::CdcAcm);
    add(CP2102N, fixtures::cp2102_raw(), SerialChip::Cp210x { partnum: 0x20 });
    usb.add(
        8,
        fixtures::flash_drive_raw(),
        &["G", "Flash", "1"],
        FakeKind::MassStorage { blocks: 64 },
        true,
    );
    agent.set_usb_backend(usb.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let a = agent.clone();
    tokio::spawn(async move { a.serve(listener).await });
    let mut store = Store::load(&temp_dir("host").join("host.json")).unwrap();
    let pin = agent.start_pairing();
    Client::pair(&[addr], &mut store, &pin).await.unwrap().close().await;
    let client = Arc::new(Client::connect(&[addr], &mut store).await.unwrap());
    Env {
        agent,
        usb,
        client,
        infos,
    }
}

impl Env {
    fn info(&self, id: u32) -> &UsbDeviceInfo {
        self.infos.iter().find(|d| d.id == id).unwrap()
    }

    async fn open(&self, id: u32) -> SerialPort {
        self.open_with(id, SerialConfig::default(), PortOptions::default())
            .await
    }

    async fn open_with(&self, id: u32, cfg: SerialConfig, opts: PortOptions) -> SerialPort {
        SerialPort::open(Arc::clone(&self.client), self.info(id), cfg, opts)
            .await
            .unwrap_or_else(|e| panic!("open {id}: {e:#}"))
    }
}

/// The control requests of the log as (request type, request, value, index).
fn requests(usb: &FakeUsb, id: u32) -> Vec<(u8, u8, u16, u16)> {
    usb.serial_log(id)
        .into_iter()
        .filter_map(|e| match e {
            SerialEvent::Control { setup, .. } => Some((setup.request_type, setup.request, setup.value, setup.index)),
            SerialEvent::Out { .. } => None,
        })
        .collect()
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

async fn read_exact(p: &SerialPort, n: usize) -> Vec<u8> {
    let rx = p.rx();
    let mut got = Vec::new();
    while got.len() < n {
        let chunk = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("data should arrive")
            .expect("port open");
        got.extend(chunk);
    }
    got
}

// --- 5.1 opening ---------------------------------------------------------------------------

#[tokio::test]
async fn open_sequences_at_115200_8n1() {
    let e = env().await;
    // CP210x: part number first, then the sequence of the spec, flow, modem lines.
    let p = e.open(CP2102).await;
    assert_eq!(
        requests(&e.usb, CP2102),
        vec![
            (0xc0, 0xff, 0x370b, 0),
            (0x41, 0x00, 1, 0),
            (0x41, 0x1e, 0, 0),
            (0x41, 0x03, 0x0800, 0),
            (0xc1, 0x14, 0, 0),
            (0x41, 0x13, 0, 0),
            (0x41, 0x07, 0x0303, 0),
        ]
    );
    let d = e.usb.serial_state(CP2102);
    assert_eq!((d.baud, d.data_bits, d.parity, d.stop_bits), (115_200, 8, 0, 0));
    assert!(d.enabled && d.dtr && d.rts);
    assert_eq!(p.info().label, "CP210x (CP2102)");
    p.close().await;

    let p = e.open(FT232R).await;
    assert_eq!(
        requests(&e.usb, FT232R),
        vec![
            (0x40, 0x00, 0, 0),
            (0x40, 0x09, 16, 0),
            (0x40, 0x03, 0x001a, 0),
            (0x40, 0x04, 0x0008, 0),
            (0x40, 0x02, 0, 0),
            (0x40, 0x01, 0x0101, 0),
            (0x40, 0x01, 0x0202, 0),
        ]
    );
    p.close().await;

    // FTDI ports address their channel: FT232H is channel 1, port B of an FT2232H channel 2.
    let p = e.open(FT232H).await;
    assert!(requests(&e.usb, FT232H).contains(&(0x40, 0x03, 0xc068, 0x0201)));
    p.close().await;
    let p = e
        .open_with(
            FT2232H,
            SerialConfig::default(),
            PortOptions {
                port: 1,
                ..Default::default()
            },
        )
        .await;
    assert!(requests(&e.usb, FT2232H).contains(&(0x40, 0x03, 0xc068, 0x0202)));
    assert_eq!(e.usb.serial_state_port(FT2232H, 1).baud / 1000, 115);
    assert_eq!(e.usb.serial_state_port(FT2232H, 0).baud, 9600, "port A untouched");
    p.close().await;

    let p = e.open(CH340).await;
    assert_eq!(
        requests(&e.usb, CH340),
        vec![
            (0xc0, 0x5f, 0, 0),
            (0xc0, 0x95, 0x1805, 0),
            (0x40, 0xa1, 0, 0),
            (0x40, 0x9a, 0x1312, 0xcc83),
            (0x40, 0x9a, 0x2518, 0x00c3),
            (0x40, 0xa4, 0xff9f, 0),
            (0xc0, 0x95, 0x0706, 0),
        ]
    );
    p.close().await;

    let p = e.open(CDC).await;
    assert_eq!(requests(&e.usb, CDC), vec![(0x21, 0x20, 0, 0), (0x21, 0x22, 3, 0)]);
    let coding = e.usb.serial_log(CDC).into_iter().find_map(|ev| match ev {
        SerialEvent::Control { setup, data } if setup.request == 0x20 => Some(data),
        _ => None,
    });
    assert_eq!(coding.unwrap(), vec![0x00, 0xc2, 0x01, 0x00, 0x00, 0x00, 0x08]);
    p.close().await;
}

#[tokio::test]
async fn open_without_dtr_asserts_nothing() {
    let e = env().await;
    let cfg = SerialConfig {
        dtr: false,
        rts: false,
        ..SerialConfig::default()
    };
    let p = e.open_with(CP2102, cfg, PortOptions::default()).await;
    let reqs = requests(&e.usb, CP2102);
    assert_eq!(reqs.last(), Some(&(0x41, 0x07, 0x0300, 0)), "both lines cleared");
    let d = e.usb.serial_state(CP2102);
    assert!(!d.dtr && !d.rts);
    p.close().await;
    let p = e.open_with(CH340, cfg, PortOptions::default()).await;
    assert!(requests(&e.usb, CH340).contains(&(0x40, 0xa4, 0xffff, 0)));
    p.close().await;
    let p = e.open_with(FT232R, cfg, PortOptions::default()).await;
    let reqs = requests(&e.usb, FT232R);
    assert!(reqs.contains(&(0x40, 0x01, 0x0100, 0)) && reqs.contains(&(0x40, 0x01, 0x0200, 0)));
    assert!(!reqs.contains(&(0x40, 0x01, 0x0101, 0)));
    p.close().await;
    let p = e.open_with(CDC, cfg, PortOptions::default()).await;
    assert!(requests(&e.usb, CDC).contains(&(0x21, 0x22, 0, 0)));
    p.close().await;
}

#[tokio::test]
async fn close_gives_the_device_back() {
    let e = env().await;
    let p = e.open(CP2102).await;
    p.close().await;
    let d = e.usb.serial_state(CP2102);
    assert!(
        !d.dtr && !d.rts && !d.enabled,
        "lines dropped, interface disabled: {d:?}"
    );
    // Another command can open the device now.
    e.client.open_device(CP2102).await.unwrap();
    e.client.close_device(CP2102).await.unwrap();
    // A second close is harmless; the port reports why it ended.
    p.close().await;
    assert_eq!(p.done().await, "port closed");
    // Not a serial device: refused before it is opened.
    let err = SerialPort::open(
        Arc::clone(&e.client),
        e.info(8),
        SerialConfig::default(),
        PortOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        format!("{err:#}").contains("not a supported USB serial device"),
        "{err:#}"
    );
    e.client.open_device(8).await.unwrap();
}

#[tokio::test]
async fn rate_out_of_range_is_refused_and_line_keeps_its_rate() {
    let e = env().await;
    // Opening a CP2102 at 3 Mbaud fails and leaves the device closed.
    let cfg = SerialConfig {
        line: Line {
            baud: 3_000_000,
            ..Line::default()
        },
        ..SerialConfig::default()
    };
    let err = SerialPort::open(Arc::clone(&e.client), e.info(CP2102), cfg, PortOptions::default())
        .await
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("1000000"), "{err:#}");
    e.client.open_device(CP2102).await.unwrap();
    e.client.close_device(CP2102).await.unwrap();

    let p = e.open(CP2102).await;
    let err = p
        .set_line(Line {
            baud: 3_000_000,
            ..Line::default()
        })
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("1000000"), "{err:#}");
    assert_eq!(p.line().baud, 115_200);
    assert_eq!(e.usb.serial_state(CP2102).baud, 115_200);
    p.close().await;
    // A CP2102N takes it.
    let p = e
        .open_with(
            CP2102N,
            SerialConfig {
                line: Line {
                    baud: 3_000_000,
                    ..Line::default()
                },
                ..SerialConfig::default()
            },
            PortOptions::default(),
        )
        .await;
    assert_eq!(e.usb.serial_state(CP2102N).baud, 3_000_000);
    p.close().await;
}

// --- 5.2 IN pump ---------------------------------------------------------------------------

#[tokio::test]
async fn idle_line_survives_in_timeouts() {
    let e = env().await;
    let p = e
        .open_with(
            CP2102,
            SerialConfig::default(),
            PortOptions {
                in_timeout: Duration::from_millis(300),
                ..Default::default()
            },
        )
        .await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(p.is_done().is_none(), "the port stays open");
    e.usb.inject_rx(CP2102, b"late data");
    assert_eq!(read_exact(&p, 9).await, b"late data");
    p.close().await;
}

#[tokio::test]
async fn stalled_in_endpoint_is_recovered() {
    let e = env().await;
    let p = e.open(CP2102).await;
    e.usb.stall_next_in(CP2102);
    tokio::time::sleep(Duration::from_millis(100)).await;
    e.usb.inject_rx(CP2102, b"after stall");
    assert_eq!(read_exact(&p, 11).await, b"after stall");
    assert!(p.is_done().is_none());
    p.close().await;
}

// --- 5.3 OUT writer ------------------------------------------------------------------------

async fn round_trip(e: &Env, id: u32, opts: PortOptions) {
    let p = e.open_with(id, SerialConfig::default(), opts).await;
    let data = pattern(64 * 1024);
    let writer = {
        let p = p.clone();
        let data = data.clone();
        tokio::spawn(async move {
            for c in data.chunks(1500) {
                p.write(c).await.unwrap();
            }
        })
    };
    let got = read_exact(&p, data.len()).await;
    writer.await.unwrap();
    assert!(got == data, "device {id}: data differs");
    p.close().await;
}

#[tokio::test]
async fn loopback_round_trip_on_every_chip() {
    let e = env().await;
    round_trip(&e, CP2102, PortOptions::default()).await;
    round_trip(&e, FT232R, PortOptions::default()).await;
    round_trip(
        &e,
        FT2232H,
        PortOptions {
            port: 1,
            ..Default::default()
        },
    )
    .await;
    round_trip(&e, FT232H, PortOptions::default()).await;
    round_trip(&e, CH340, PortOptions::default()).await;
    round_trip(&e, CDC, PortOptions::default()).await;
}

#[tokio::test]
async fn line_change_waits_for_earlier_writes() {
    let e = env().await;
    let p = e.open(CP2102).await;
    e.usb.clear_serial_log(CP2102);
    let mut total = 0;
    for _ in 0..6 {
        p.write(&pattern(5000)).await.unwrap();
        total += 5000;
    }
    p.set_line(Line {
        baud: 57_600,
        ..Line::default()
    })
    .await
    .unwrap();
    let log = e.usb.serial_log(CP2102);
    let change = log
        .iter()
        .position(|ev| matches!(ev, SerialEvent::Control { setup, .. } if setup.request == 0x1e))
        .expect("baud request logged");
    let sent: usize = log[..change]
        .iter()
        .map(|ev| if let SerialEvent::Out { len } = ev { *len } else { 0 })
        .sum();
    assert_eq!(sent, total, "all earlier OUT data left before the line change");
    assert_eq!(e.usb.serial_state(CP2102).baud, 57_600);
    p.close().await;
}

// --- 5.4 line, lines, break, purge ------------------------------------------------------------

#[tokio::test]
async fn line_format_and_flow_per_chip() {
    let e = env().await;

    let p = e.open(FT232R).await;
    let eff = p
        .set_line(Line {
            baud: 115_200,
            data_bits: 7,
            parity: Parity::Even,
            stop_bits: StopBits::One,
        })
        .await
        .unwrap();
    assert_eq!((eff.data_bits, eff.parity), (7, Parity::Even));
    assert!(requests(&e.usb, FT232R).contains(&(0x40, 0x04, 0x0207, 0)));
    // 5 data bits: the chip does 8, and says so.
    let eff = p
        .set_line(Line {
            data_bits: 5,
            ..Line::default()
        })
        .await
        .unwrap();
    assert_eq!(eff.data_bits, 8);
    assert_eq!(p.line().data_bits, 8);
    p.close().await;

    let p = e.open(CP2102).await;
    assert_eq!(p.set_flow(Flow::RtsCts).await.unwrap(), Flow::RtsCts);
    assert!(e.usb.serial_state(CP2102).flow_rtscts);
    p.set_line(Line {
        baud: 9600,
        data_bits: 7,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
    })
    .await
    .unwrap();
    assert!(requests(&e.usb, CP2102).contains(&(0x41, 0x03, 0x0722, 0)));
    // RTS goes through the flow block while RTS/CTS is on.
    e.usb.clear_serial_log(CP2102);
    p.set_rts(false).await.unwrap();
    assert_eq!(requests(&e.usb, CP2102), vec![(0xc1, 0x14, 0, 0), (0x41, 0x13, 0, 0)]);
    p.close().await;

    let p = e.open(CH340).await;
    p.set_line(Line {
        baud: 9600,
        data_bits: 7,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
    })
    .await
    .unwrap();
    let reqs = requests(&e.usb, CH340);
    assert!(reqs.contains(&(0x40, 0x9a, 0x1312, 0xb282)));
    assert!(reqs.contains(&(0x40, 0x9a, 0x2518, 0x00c0 | 0x02 | 0x18 | 0x04)));
    // No hardware flow control in this driver: the setting in effect comes back.
    assert_eq!(p.set_flow(Flow::RtsCts).await.unwrap(), Flow::None);
    p.close().await;
}

#[tokio::test]
async fn modem_lines_and_break() {
    let e = env().await;
    let cfg = SerialConfig {
        dtr: false,
        rts: false,
        ..SerialConfig::default()
    };
    let p = e.open_with(CH340, cfg, PortOptions::default()).await;
    p.set_dtr(true).await.unwrap();
    p.set_rts(true).await.unwrap();
    assert_eq!(requests(&e.usb, CH340).last(), Some(&(0x40, 0xa4, 0xff9f, 0)));
    p.set_break(true).await.unwrap();
    assert!(e.usb.serial_state(CH340).break_on);
    p.set_break(false).await.unwrap();
    assert!(!e.usb.serial_state(CH340).break_on);
    p.close().await;

    let p = e.open_with(FT232R, cfg, PortOptions::default()).await;
    p.set_dtr(true).await.unwrap();
    p.set_dtr(false).await.unwrap();
    assert_eq!(requests(&e.usb, FT232R).last(), Some(&(0x40, 0x01, 0x0100, 0)));
    p.set_break(true).await.unwrap();
    assert!(e.usb.serial_state(FT232R).break_on);
    p.close().await;

    let p = e.open(CDC).await;
    e.usb.clear_serial_log(CDC);
    p.set_break(true).await.unwrap();
    p.set_break(false).await.unwrap();
    assert_eq!(requests(&e.usb, CDC), vec![(0x21, 0x23, 0xffff, 0), (0x21, 0x23, 0, 0)]);
    p.close().await;
}

#[tokio::test]
async fn purge_clears_host_buffer_and_asks_the_chip() {
    let e = env().await;
    let p = e.open(CP2102).await;
    e.usb.inject_rx(CP2102, b"stale bytes");
    for _ in 0..50 {
        if p.rx().buffered() == 11 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(p.rx().buffered(), 11);
    e.usb.clear_serial_log(CP2102);
    p.purge(true, false).await.unwrap();
    assert_eq!(p.rx().buffered(), 0);
    assert_eq!(requests(&e.usb, CP2102), vec![(0x41, 0x12, 0x000a, 0)]);
    p.close().await;

    let p = e.open(FT232R).await;
    e.usb.clear_serial_log(FT232R);
    p.purge(true, true).await.unwrap();
    assert_eq!(requests(&e.usb, FT232R), vec![(0x40, 0x00, 1, 0), (0x40, 0x00, 2, 0)]);
    p.close().await;

    // CH34x and CDC have no purge request: only the host buffers are cleared.
    let p = e.open(CH340).await;
    e.usb.clear_serial_log(CH340);
    p.purge(true, true).await.unwrap();
    assert!(requests(&e.usb, CH340).is_empty());
    p.close().await;
}

// --- 5.5 modem status and line errors ---------------------------------------------------------

async fn next_modem(rx: &mut tokio::sync::broadcast::Receiver<PortEvent>) -> hubbridge_host::serial::ModemStatus {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Ok(PortEvent::Modem(m))) => return m,
            Ok(Ok(_)) => {}
            other => panic!("no modem event: {other:?}"),
        }
    }
}

#[tokio::test]
async fn modem_events_from_cdc_ch340_and_ftdi() {
    let e = env().await;
    for id in [CDC, CH340, FT232R] {
        let p = e.open(id).await;
        let mut rx = p.subscribe();
        e.usb.set_modem(
            id,
            ModemLines {
                dcd: true,
                dsr: true,
                ..Default::default()
            },
        );
        let m = next_modem(&mut rx).await;
        assert!(m.dcd && m.dsr && !m.ri, "device {id}: {m:?}");
        assert!(p.modem_status().dcd);
        e.usb.set_modem(id, ModemLines::default());
        let m = next_modem(&mut rx).await;
        assert!(!m.dcd && !m.dsr, "device {id}: {m:?}");
        p.close().await;
    }
}

#[tokio::test]
async fn ftdi_overrun_is_counted_and_cdc_framing_reported() {
    let e = env().await;
    let p = e.open(FT232R).await;
    e.usb.inject_line_error(FT232R, LineErrorKind::Overrun);
    p.write(b"x").await.unwrap();
    assert_eq!(read_exact(&p, 1).await, b"x");
    assert_eq!(p.stats().overruns, 1);
    p.close().await;

    let p = e.open(CDC).await;
    let mut rx = p.subscribe();
    e.usb.inject_line_error(CDC, LineErrorKind::Framing);
    loop {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Ok(PortEvent::LineError(le))) => {
                assert!(le.framing);
                break;
            }
            Ok(Ok(_)) => {}
            other => panic!("no line error: {other:?}"),
        }
    }
    assert_eq!(p.stats().framing, 1);
    p.close().await;
}

#[tokio::test]
async fn cp210x_polls_modem_status_only_on_demand() {
    let e = env().await;
    let p = e.open(CP2102).await;
    e.usb.clear_serial_log(CP2102);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        requests(&e.usb, CP2102).iter().all(|r| r.1 != 0x08),
        "no GET_MDMSTS without demand"
    );
    let mut rx = p.subscribe();
    p.want_modem_status(true);
    e.usb.set_modem(
        CP2102,
        ModemLines {
            cts: true,
            ..Default::default()
        },
    );
    let m = next_modem(&mut rx).await;
    assert!(m.cts);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let polls = requests(&e.usb, CP2102).iter().filter(|r| r.1 == 0x08).count();
    assert!(polls >= 3, "polled {polls} times");
    p.want_modem_status(false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    e.usb.clear_serial_log(CP2102);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(requests(&e.usb, CP2102).iter().all(|r| r.1 != 0x08));
    // Overruns of a CP210x come from GET_COMM_STATUS.
    e.usb.inject_line_error(CP2102, LineErrorKind::Overrun);
    p.refresh_errors().await.unwrap();
    assert_eq!(p.stats().overruns, 1);
    p.close().await;
}

// --- 5.6 counters and end ---------------------------------------------------------------------

#[tokio::test]
async fn counters_after_a_round_trip() {
    let e = env().await;
    let p = e.open(CP2102).await;
    p.write(&pattern(1000)).await.unwrap();
    assert_eq!(read_exact(&p, 1000).await, pattern(1000));
    p.drain().await;
    let s = p.stats();
    assert_eq!((s.tx, s.rx, s.rx_dropped), (1000, 1000, 0));
    assert_eq!(s.line_errors(), 0);
    p.close().await;
}

#[tokio::test]
async fn unplugging_the_device_stops_the_port() {
    let e = env().await;
    let p = e.open(CP2102).await;
    let start = std::time::Instant::now();
    let rest: Vec<_> = e.infos.iter().filter(|d| d.id != CP2102).cloned().collect();
    e.agent.set_devices(rest);
    let reason = tokio::time::timeout(Duration::from_secs(2), p.done())
        .await
        .expect("the port stops within 2 s");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(reason.contains("device closed"), "{reason}");
    assert!(p.write(b"x").await.is_err());
    p.close().await;
}

#[tokio::test]
async fn consumer_that_does_not_read_loses_bytes_beyond_the_buffer() {
    let e = env().await;
    let p = e.open(CP2102).await;
    // 1.5 MiB arrives while nobody reads: the first MiB is kept, the rest counted as dropped.
    let chunk = vec![0x55u8; 64 * 1024];
    for _ in 0..24 {
        e.usb.inject_rx(CP2102, &chunk);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for _ in 0..200 {
        let s = p.stats();
        if s.rx + s.rx_dropped >= 24 * 64 * 1024 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = p.stats();
    assert_eq!(s.rx + s.rx_dropped, 24 * 64 * 1024);
    assert_eq!(s.rx, 1 << 20);
    assert_eq!(s.rx_dropped, 24 * 64 * 1024 - (1 << 20));
    assert!(p.is_done().is_none(), "dropping does not stop the pump");
    p.close().await;
}

// --- 6.2 / 6.3 pseudo-terminal front end -----------------------------------------------------

#[cfg(unix)]
mod pty {
    use std::os::fd::OwnedFd;

    use hubbridge_host::serial::pty::{PtyFront, PtyOptions};
    use rustix::fs::{Mode, OFlags, open};
    use rustix::termios::{OptionalActions, QueueSelector, tcflush, tcgetattr, tcsetattr};

    use super::*;

    /// What an application does: open the slave path.
    fn open_slave(path: &str) -> OwnedFd {
        open(path, OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK, Mode::empty()).expect("open the PTY slave")
    }

    fn app_write(fd: &OwnedFd, data: &[u8]) {
        let mut data = data;
        while !data.is_empty() {
            match rustix::io::write(fd, data) {
                Ok(n) => data = &data[n..],
                Err(rustix::io::Errno::AGAIN) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => panic!("write to the PTY: {e}"),
            }
        }
    }

    async fn app_read(fd: &OwnedFd, n: usize) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        let start = std::time::Instant::now();
        while got.len() < n {
            match rustix::io::read(fd, &mut buf) {
                Ok(k) => got.extend_from_slice(&buf[..k]),
                Err(rustix::io::Errno::AGAIN) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(e) => panic!("read from the PTY: {e}"),
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "PTY read timed out ({} bytes)",
                got.len()
            );
        }
        got
    }

    async fn wait_for<F: FnMut() -> bool>(mut f: F) {
        for _ in 0..400 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached");
    }

    #[tokio::test]
    async fn echo_and_reopen() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let pty = PtyFront::start(p.clone(), PtyOptions::default()).unwrap();
        assert!(pty.path().starts_with("/dev/"), "{}", pty.path());
        let app = open_slave(pty.path());
        app_write(&app, b"hello");
        assert_eq!(app_read(&app, 5).await, b"hello");
        // Closing and reopening does not stop the bridge.
        drop(app);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let app = open_slave(pty.path());
        app_write(&app, b"again");
        assert_eq!(app_read(&app, 5).await, b"again");
        assert!(p.is_done().is_none());
        // Data from the device reaches the application too.
        e.usb.inject_rx(CP2102, b"from device");
        assert_eq!(app_read(&app, 11).await, b"from device");
        pty.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn flush_of_the_pty_purges_the_port() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let pty = PtyFront::start(p.clone(), PtyOptions::default()).unwrap();
        let app = open_slave(pty.path());
        e.usb.clear_serial_log(CP2102);
        tcflush(&app, QueueSelector::IFlush).unwrap();
        wait_for(|| requests(&e.usb, CP2102).contains(&(0x41, 0x12, 0x000a, 0))).await;
        e.usb.clear_serial_log(CP2102);
        tcflush(&app, QueueSelector::OFlush).unwrap();
        wait_for(|| requests(&e.usb, CP2102).contains(&(0x41, 0x12, 0x0005, 0))).await;
        pty.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn line_change_comes_before_the_data_that_follows() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let pty = PtyFront::start(p.clone(), PtyOptions::default()).unwrap();
        let app = open_slave(pty.path());
        e.usb.clear_serial_log(CP2102);
        let mut t = tcgetattr(&app).unwrap();
        t.set_speed(57_600).unwrap();
        t.control_modes |= rustix::termios::ControlModes::PARENB;
        tcsetattr(&app, OptionalActions::Now, &t).unwrap();
        app_write(&app, b"x");
        wait_for(|| {
            e.usb
                .serial_log(CP2102)
                .iter()
                .any(|ev| matches!(ev, SerialEvent::Out { .. }))
        })
        .await;
        let log = e.usb.serial_log(CP2102);
        let out = log.iter().position(|ev| matches!(ev, SerialEvent::Out { .. })).unwrap();
        let baud = log
            .iter()
            .position(|ev| matches!(ev, SerialEvent::Control { setup, .. } if setup.request == 0x1e))
            .expect("baud change logged");
        let line = log
            .iter()
            .position(|ev| matches!(ev, SerialEvent::Control { setup, .. } if setup.request == 0x03))
            .expect("line control logged");
        assert!(baud < out && line < out, "line change before the data: {log:?}");
        let d = e.usb.serial_state(CP2102);
        assert_eq!((d.baud, d.parity), (57_600, 2), "even parity");
        assert_eq!(p.line().baud, 57_600);
        pty.stop();
        p.close().await;
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn nonstandard_rate_reaches_the_chip() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let pty = PtyFront::start(p.clone(), PtyOptions::default()).unwrap();
        let app = open_slave(pty.path());
        let mut t = tcgetattr(&app).unwrap();
        t.set_speed(250_000).unwrap();
        tcsetattr(&app, OptionalActions::Now, &t).unwrap();
        wait_for(|| e.usb.serial_state(CP2102).baud == 250_000).await;
        pty.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn locked_line_ignores_the_application() {
        let e = env().await;
        let cfg = SerialConfig {
            line: Line {
                baud: 921_600,
                ..Line::default()
            },
            ..SerialConfig::default()
        };
        // CP2102N takes 921600.
        let p = e.open_with(CP2102N, cfg, PortOptions::default()).await;
        let pty = PtyFront::start(p.clone(), PtyOptions { lock_line: true }).unwrap();
        let app = open_slave(pty.path());
        let mut t = tcgetattr(&app).unwrap();
        t.set_speed(9600).unwrap();
        tcsetattr(&app, OptionalActions::Now, &t).unwrap();
        app_write(&app, b"data");
        assert_eq!(app_read(&app, 4).await, b"data");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(e.usb.serial_state(CP2102N).baud, 921_600);
        pty.stop();
        p.close().await;
    }
}
