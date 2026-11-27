//! Serial ports end to end over real TCP: host serial core ↔ agent core ↔ fake serial chips.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::PausableProxy;
use hubbridge_agent::fake::{FakeKind, FakeUsb, LineErrorKind, ModemLines, SerialChip, SerialEvent};
use hubbridge_agent::{Agent, AgentConfig};
use hubbridge_host::client::AgentError;
use hubbridge_host::serial::{Flow, Line, Parity, PortEvent, PortOptions, SerialConfig, SerialPort, StopBits};
use hubbridge_host::{Client, Store};
use hubbridge_proto::{ErrorCode, Message, UsbDeviceInfo, fixtures};

const CP2102: u32 = 1;
const FT232R: u32 = 2;
const FT2232H: u32 = 3;
const FT232H: u32 = 4;
const CH340: u32 = 5;
const CDC: u32 = 6;
const CP2102N: u32 = 7;
const CH340_LIMITED: u32 = 9;

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
    /// Set to stop the proxy between host and agent from passing anything on (a half-open
    /// session); only with [`env_proxied`].
    freeze: Arc<std::sync::atomic::AtomicBool>,
    /// Where the agent listens (to start another session).
    addr: std::net::SocketAddr,
    /// The host's key store (to connect another client).
    store_path: PathBuf,
}

async fn env() -> Env {
    env_with(false).await
}

/// A TCP proxy that can be frozen sits between the host and the agent.
async fn env_proxied() -> Env {
    env_with(true).await
}

/// Copy bytes one way; while `freeze` is set nothing is passed on (and nothing closes).
async fn pipe<R, W>(mut from: R, mut to: W, freeze: Arc<std::sync::atomic::AtomicBool>)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let Ok(n) = from.read(&mut buf).await else { return };
        if n == 0 {
            return;
        }
        while freeze.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

async fn proxy(upstream: std::net::SocketAddr, freeze: Arc<std::sync::atomic::AtomicBool>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((down, _)) = listener.accept().await else { return };
            let Ok(up) = tokio::net::TcpStream::connect(upstream).await else {
                return;
            };
            let ((dr, dw), (ur, uw)) = (down.into_split(), up.into_split());
            tokio::spawn(pipe(dr, uw, Arc::clone(&freeze)));
            tokio::spawn(pipe(ur, dw, Arc::clone(&freeze)));
        }
    });
    addr
}

async fn env_with(proxied: bool) -> Env {
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
        fixtures::ch340_uart(CH340_LIMITED),
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
    add(
        CH340_LIMITED,
        fixtures::ch340_raw(),
        SerialChip::Ch34x {
            version: 0x31,
            limited_prescaler: true,
        },
    );
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
    let freeze = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let via = if proxied {
        proxy(addr, Arc::clone(&freeze)).await
    } else {
        addr
    };
    let client = Arc::new(Client::connect(&[via], &mut store).await.unwrap());
    Env {
        agent,
        usb,
        client,
        infos,
        freeze,
        addr,
        store_path: store.path().to_path_buf(),
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

/// Wait until `p` has counted `n` bytes as sent: the counter moves when the OUT transfer
/// completes (a spawned task), which can be after the echo came back.
async fn wait_tx(p: &SerialPort, n: u64) {
    for _ in 0..500 {
        if p.stats().tx >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("tx counter stayed at {} (wanted {n})", p.stats().tx);
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
            (0xc0, 0x95, 0x0005, 0),
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
    let mut polls = 0;
    for _ in 0..500 {
        polls = requests(&e.usb, CP2102).iter().filter(|r| r.1 == 0x08).count();
        if polls >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

#[cfg(target_os = "macos")]
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

// --- 6.4 raw TCP, 6.6 RFC 2217, 6.7 the bridge -------------------------------------------------

mod net {
    use std::collections::VecDeque;
    use std::net::SocketAddr;

    use hubbridge_host::serial::bridge::{BridgeOptions, SerialBridge};
    use hubbridge_host::serial::rfc2217 as r;
    use hubbridge_host::serial::tcp::{TcpFront, TcpOptions};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    const LOCAL: &str = "127.0.0.1:0";

    async fn raw_front(e: &Env, id: u32) -> (SerialPort, TcpFront) {
        let p = e.open(id).await;
        let t = TcpFront::start(p.clone(), LOCAL.parse().unwrap(), TcpOptions::default())
            .await
            .unwrap();
        (p, t)
    }

    async fn read_n(s: &mut TcpStream, n: usize) -> Vec<u8> {
        let mut got = vec![0u8; n];
        tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut got))
            .await
            .expect("bytes should arrive")
            .unwrap();
        got
    }

    #[tokio::test]
    async fn raw_client_round_trip_keeps_every_byte() {
        let e = env().await;
        let (p, t) = raw_front(&e, CP2102).await;
        let mut c = TcpStream::connect(t.addr()).await.unwrap();
        let all: Vec<u8> = (0..=255u8).collect();
        c.write_all(&all).await.unwrap();
        assert_eq!(read_n(&mut c, 256).await, all);
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn second_client_is_closed_and_first_keeps_working() {
        let e = env().await;
        let (p, t) = raw_front(&e, CP2102).await;
        let mut first = TcpStream::connect(t.addr()).await.unwrap();
        first.write_all(b"one").await.unwrap();
        assert_eq!(read_n(&mut first, 3).await, b"one");
        let mut second = TcpStream::connect(t.addr()).await.unwrap();
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(3), second.read(&mut buf))
            .await
            .expect("the second connection is closed")
            .unwrap_or(0);
        assert_eq!(n, 0);
        first.write_all(b"two").await.unwrap();
        assert_eq!(read_n(&mut first, 3).await, b"two");
        // After the first leaves, a new client is served (retry until the server noticed).
        drop(first);
        let mut served = false;
        for _ in 0..100 {
            let mut third = TcpStream::connect(t.addr()).await.unwrap();
            third.write_all(b"three").await.unwrap();
            let mut buf = [0u8; 5];
            if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(200), third.read_exact(&mut buf)).await
                && &buf == b"three"
            {
                served = true;
                break;
            }
        }
        assert!(served, "a new client is served once the first one is gone");
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn data_while_nobody_is_connected_is_not_kept() {
        let e = env().await;
        let (p, t) = raw_front(&e, CP2102).await;
        e.usb.inject_rx(CP2102, b"nobody listens");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut c = TcpStream::connect(t.addr()).await.unwrap();
        c.write_all(b"fresh").await.unwrap();
        assert_eq!(read_n(&mut c, 5).await, b"fresh");
        assert_eq!(p.stats().rx_dropped, 0);
        t.stop();
        p.close().await;
    }

    // A small RFC 2217 client.
    struct Rfc {
        s: TcpStream,
        parser: r::Parser,
        data: Vec<u8>,
        subs: VecDeque<Vec<u8>>,
        raw: Vec<u8>,
        negotiations: Vec<(r::Verb, u8)>,
    }

    impl Rfc {
        async fn connect(addr: SocketAddr) -> Rfc {
            let mut c = Rfc {
                s: TcpStream::connect(addr).await.unwrap(),
                parser: r::Parser::new(),
                data: Vec::new(),
                subs: VecDeque::new(),
                raw: Vec::new(),
                negotiations: Vec::new(),
            };
            // Read the server's opening negotiation and agree.
            while c.negotiations.len() < 4 {
                c.pump().await;
            }
            for (verb, opt) in c.negotiations.clone() {
                let answer = match (verb, opt) {
                    (r::Verb::Will, o) => r::negotiate(r::Verb::Do, o),
                    (r::Verb::Do, o) => r::negotiate(r::Verb::Will, o),
                    _ => continue,
                };
                c.s.write_all(&answer).await.unwrap();
            }
            c
        }

        async fn pump(&mut self) {
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(5), self.s.read(&mut buf))
                .await
                .expect("the server answers")
                .unwrap();
            assert!(n > 0, "server closed the connection");
            self.raw.extend_from_slice(&buf[..n]);
            for ev in self.parser.feed(&buf[..n]) {
                match ev {
                    r::Event::Data(d) => self.data.extend(d),
                    r::Event::Negotiate { verb, option } => self.negotiations.push((verb, option)),
                    r::Event::Sub { option, data } => {
                        if option == r::OPT_COM_PORT {
                            self.subs.push_back(data);
                        }
                    }
                }
            }
        }

        async fn cmd(&mut self, command: u8, args: &[u8]) {
            self.s.write_all(&r::com_port(command, args)).await.unwrap();
        }

        /// The next sub-negotiation with this code (others stay queued).
        async fn sub(&mut self, code: u8) -> Vec<u8> {
            loop {
                if let Some(i) = self.subs.iter().position(|s| s[0] == code) {
                    return self.subs.remove(i).unwrap()[1..].to_vec();
                }
                self.pump().await;
            }
        }

        async fn no_sub(&mut self, code: u8, wait: Duration) -> bool {
            let end = tokio::time::Instant::now() + wait;
            while tokio::time::Instant::now() < end {
                let left = end - tokio::time::Instant::now();
                let mut buf = [0u8; 4096];
                if let Ok(Ok(n)) = tokio::time::timeout(left, self.s.read(&mut buf)).await {
                    for ev in self.parser.feed(&buf[..n]) {
                        match ev {
                            r::Event::Data(d) => self.data.extend(d),
                            r::Event::Sub { option: 44, data } => self.subs.push_back(data),
                            _ => {}
                        }
                    }
                }
            }
            !self.subs.iter().any(|s| s[0] == code)
        }

        async fn data(&mut self, n: usize) -> Vec<u8> {
            while self.data.len() < n {
                self.pump().await;
            }
            self.data.drain(..n).collect()
        }
    }

    async fn rfc_front(e: &Env, id: u32, lock_line: bool) -> (SerialPort, TcpFront, Rfc) {
        let p = e.open(id).await;
        let t = TcpFront::start(
            p.clone(),
            LOCAL.parse().unwrap(),
            TcpOptions {
                rfc2217: true,
                lock_line,
                label: "test".into(),
            },
        )
        .await
        .unwrap();
        let c = Rfc::connect(t.addr()).await;
        (p, t, c)
    }

    #[tokio::test]
    async fn rfc2217_negotiation_and_signature() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CP2102, false).await;
        // RFC 2217 section 1: the access server asks DO, the client answers WILL.
        assert!(c.negotiations.contains(&(r::Verb::Do, r::OPT_COM_PORT)));
        assert!(!c.negotiations.contains(&(r::Verb::Will, r::OPT_COM_PORT)));
        assert!(c.negotiations.contains(&(r::Verb::Will, r::OPT_BINARY)));
        assert!(c.negotiations.contains(&(r::Verb::Do, r::OPT_BINARY)));
        assert!(c.negotiations.contains(&(r::Verb::Will, r::OPT_SGA)));
        c.cmd(r::CMD_SIGNATURE, &[]).await;
        let sig = c.sub(100).await;
        assert!(String::from_utf8_lossy(&sig).starts_with("HubBridge "));
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_set_baud_rate_replies_after_the_device_took_it() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CP2102, false).await;
        c.s.write_all(&[0xff, 0xfa, 0x2c, 0x01, 0x00, 0x01, 0xc2, 0x00, 0xff, 0xf0])
            .await
            .unwrap();
        // Exactly the bytes of the spec.
        let want = [0xff, 0xfa, 0x2c, 0x65, 0x00, 0x01, 0xc2, 0x00, 0xff, 0xf0];
        while !c.raw.windows(want.len()).any(|w| w == want) {
            c.pump().await;
        }
        assert_eq!(
            e.usb.serial_state(CP2102).baud,
            115_200,
            "the device had it when the reply arrived"
        );
        assert_eq!(c.sub(101).await, 115_200u32.to_be_bytes());
        c.cmd(r::CMD_SET_BAUDRATE, &57_600u32.to_be_bytes()).await;
        assert_eq!(c.sub(101).await, 57_600u32.to_be_bytes());
        assert_eq!(e.usb.serial_state(CP2102).baud, 57_600);
        // Data size, parity, stop size.
        c.cmd(r::CMD_SET_DATASIZE, &[7]).await;
        assert_eq!(c.sub(102).await, vec![7]);
        c.cmd(r::CMD_SET_PARITY, &[3]).await;
        assert_eq!(c.sub(103).await, vec![3]);
        c.cmd(r::CMD_SET_STOPSIZE, &[2]).await;
        assert_eq!(c.sub(104).await, vec![2]);
        let d = e.usb.serial_state(CP2102);
        assert_eq!((d.data_bits, d.parity, d.stop_bits), (7, 2, 2));
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_query_changes_nothing() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CP2102, false).await;
        let before = e.usb.serial_log(CP2102).len();
        c.cmd(r::CMD_SET_DATASIZE, &[0]).await;
        assert_eq!(c.sub(102).await, vec![8]);
        c.cmd(r::CMD_SET_BAUDRATE, &0u32.to_be_bytes()).await;
        assert_eq!(c.sub(101).await, 115_200u32.to_be_bytes());
        c.cmd(r::CMD_SET_PARITY, &[0]).await;
        assert_eq!(c.sub(103).await, vec![1]);
        c.cmd(r::CMD_SET_STOPSIZE, &[0]).await;
        assert_eq!(c.sub(104).await, vec![1]);
        assert_eq!(e.usb.serial_log(CP2102).len(), before, "the device got no request");
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_escapes_ff_both_ways() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CP2102, false).await;
        // From the device: one 0xff arrives doubled.
        e.usb.inject_rx(CP2102, &[0x01, 0xff, 0x02]);
        c.raw.clear();
        assert_eq!(c.data(3).await, vec![0x01, 0xff, 0x02]);
        assert!(
            c.raw.windows(2).any(|w| w == [0xff, 0xff]),
            "doubled on the wire: {:?}",
            c.raw
        );
        // To the device: ff ff is one byte.
        e.usb.clear_serial_log(CP2102);
        c.s.write_all(&[b'a', 0xff, 0xff, b'b']).await.unwrap();
        assert_eq!(c.data(3).await, vec![b'a', 0xff, b'b'], "looped back, unescaped");
        let sent: usize = e
            .usb
            .serial_log(CP2102)
            .iter()
            .map(|ev| if let SerialEvent::Out { len } = ev { *len } else { 0 })
            .sum();
        assert_eq!(sent, 3);
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_dtr_rts_break_and_flow() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CP2102, false).await;
        c.cmd(r::CMD_SET_CONTROL, &[9]).await;
        assert_eq!(c.sub(105).await, vec![9]);
        c.cmd(r::CMD_SET_CONTROL, &[11]).await;
        assert_eq!(c.sub(105).await, vec![11]);
        c.cmd(r::CMD_SET_CONTROL, &[5]).await;
        assert_eq!(c.sub(105).await, vec![5]);
        let d = e.usb.serial_state(CP2102);
        assert!(!d.dtr && d.rts && d.break_on);
        // Queries.
        c.cmd(r::CMD_SET_CONTROL, &[7]).await;
        assert_eq!(c.sub(105).await, vec![9]);
        c.cmd(r::CMD_SET_CONTROL, &[4]).await;
        assert_eq!(c.sub(105).await, vec![5]);
        // XON/XOFF is not supported: the flow in effect comes back.
        c.cmd(r::CMD_SET_CONTROL, &[2]).await;
        assert_eq!(c.sub(105).await, vec![1]);
        c.cmd(r::CMD_SET_CONTROL, &[3]).await;
        assert_eq!(c.sub(105).await, vec![3]);
        assert!(e.usb.serial_state(CP2102).flow_rtscts);
        c.cmd(r::CMD_SET_CONTROL, &[13]).await;
        assert_eq!(c.sub(105).await, vec![16]);
        // Purge.
        e.usb.clear_serial_log(CP2102);
        c.cmd(r::CMD_PURGE_DATA, &[3]).await;
        assert_eq!(c.sub(112).await, vec![3]);
        assert!(requests(&e.usb, CP2102).contains(&(0x41, 0x12, 0x000f, 0)));
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_modem_and_line_notifications() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CDC, false).await;
        e.usb.set_modem(
            CDC,
            ModemLines {
                dcd: true,
                ..Default::default()
            },
        );
        let n = c.sub(107).await;
        assert_eq!(n[0] & 0x88, 0x88, "DCD and delta DCD: {n:?}");
        // A framing error with the default line-state mask (0): no notification.
        e.usb.inject_line_error(CDC, LineErrorKind::Framing);
        assert!(c.no_sub(106, Duration::from_millis(400)).await);
        // With the mask set it is reported.
        c.cmd(r::CMD_SET_LINESTATE_MASK, &[0x08]).await;
        assert_eq!(c.sub(110).await, vec![0x08]);
        e.usb.inject_line_error(CDC, LineErrorKind::Framing);
        assert_eq!(c.sub(106).await, vec![0x08]);
        // A modem mask that excludes DCD silences it.
        c.cmd(r::CMD_SET_MODEMSTATE_MASK, &[0x11]).await;
        assert_eq!(c.sub(111).await, vec![0x11]);
        let _ = c.sub(107).await; // the current state, sent once when the mask is set
        e.usb.set_modem(CDC, ModemLines::default());
        assert!(c.no_sub(107, Duration::from_millis(400)).await);
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_notification_carries_state_and_mask() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CDC, false).await;
        c.cmd(r::CMD_SET_MODEMSTATE_MASK, &[0x88]).await;
        assert_eq!(c.sub(111).await, vec![0x88]);
        let _ = c.sub(107).await;
        e.usb.set_modem(
            CDC,
            ModemLines {
                dcd: true,
                dsr: true,
                ..Default::default()
            },
        );
        // DSR (0x20, 0x02) is outside the mask: only DCD and delta DCD are sent.
        assert_eq!(c.sub(107).await, vec![0x88]);
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_data_size_must_be_5_to_8() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CP2102, false).await;
        let before = e.usb.serial_log(CP2102).len();
        for bad in [4u8, 9, 200] {
            c.cmd(r::CMD_SET_DATASIZE, &[bad]).await;
            assert_eq!(
                c.sub(102).await,
                vec![8],
                "{bad} is ignored, the size in effect comes back"
            );
        }
        assert_eq!(e.usb.serial_log(CP2102).len(), before, "nothing reached the device");
        c.cmd(r::CMD_SET_DATASIZE, &[6]).await;
        assert_eq!(c.sub(102).await, vec![6]);
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn client_that_sends_will_first_is_served() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let t = TcpFront::start(
            p.clone(),
            LOCAL.parse().unwrap(),
            TcpOptions {
                rfc2217: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Do not wait for the hello: send WILL COM-PORT-OPTION and a command at once.
        let mut c = Rfc {
            s: TcpStream::connect(t.addr()).await.unwrap(),
            parser: r::Parser::new(),
            data: Vec::new(),
            subs: VecDeque::new(),
            raw: Vec::new(),
            negotiations: Vec::new(),
        };
        c.s.write_all(&r::negotiate(r::Verb::Will, r::OPT_COM_PORT))
            .await
            .unwrap();
        c.cmd(r::CMD_SET_DATASIZE, &[7]).await;
        assert_eq!(c.sub(102).await, vec![7]);
        // The server never answers our WILL with DONT.
        assert!(!c.negotiations.contains(&(r::Verb::Dont, r::OPT_COM_PORT)));
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_flow_control_suspend_and_resume() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CP2102, false).await;
        c.cmd(r::CMD_FLOWCONTROL_SUSPEND, &[]).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        e.usb.inject_rx(CP2102, b"held back");
        let quiet = tokio::time::timeout(Duration::from_millis(400), c.pump()).await;
        assert!(quiet.is_err() && c.data.is_empty(), "nothing is sent while suspended");
        c.cmd(r::CMD_FLOWCONTROL_RESUME, &[]).await;
        assert_eq!(c.data(9).await, b"held back");
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn rfc2217_cp210x_modem_polling_follows_the_client() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let t = TcpFront::start(
            p.clone(),
            LOCAL.parse().unwrap(),
            TcpOptions {
                rfc2217: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut c = Rfc::connect(t.addr()).await;
        e.usb.clear_serial_log(CP2102);
        e.usb.set_modem(
            CP2102,
            ModemLines {
                dsr: true,
                ..Default::default()
            },
        );
        // The default mask is not 0: the port polls and the client hears about DSR.
        let n = c.sub(107).await;
        assert_eq!(n[0] & 0x22, 0x22, "{n:?}");
        // A client that wants no modem status stops the polling.
        c.cmd(r::CMD_SET_MODEMSTATE_MASK, &[0]).await;
        assert_eq!(c.sub(111).await, vec![0]);
        tokio::time::sleep(Duration::from_millis(400)).await;
        e.usb.clear_serial_log(CP2102);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(requests(&e.usb, CP2102).iter().all(|q| q.1 != 0x08));
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn locked_line_answers_with_the_current_values() {
        let e = env().await;
        let (p, t, mut c) = rfc_front(&e, CP2102, true).await;
        c.cmd(r::CMD_SET_BAUDRATE, &9600u32.to_be_bytes()).await;
        assert_eq!(c.sub(101).await, 115_200u32.to_be_bytes());
        c.cmd(r::CMD_SET_PARITY, &[3]).await;
        assert_eq!(c.sub(103).await, vec![1]);
        c.cmd(r::CMD_SET_DATASIZE, &[7]).await;
        assert_eq!(c.sub(102).await, vec![8]);
        assert_eq!(e.usb.serial_state(CP2102).baud, 115_200);
        // DTR is not part of the line: it still works (auto-reset of boards).
        c.cmd(r::CMD_SET_CONTROL, &[9]).await;
        assert_eq!(c.sub(105).await, vec![9]);
        assert!(!e.usb.serial_state(CP2102).dtr);
        t.stop();
        p.close().await;
    }

    #[tokio::test]
    async fn bridge_serves_pty_and_rfc2217_together_and_stop_gives_the_device_back() {
        let e = env().await;
        let opts = BridgeOptions {
            pty: cfg!(target_os = "macos"),
            tcp: Some(LOCAL.parse().unwrap()),
            rfc2217: true,
            ..BridgeOptions::default()
        };
        let b = SerialBridge::start(Arc::clone(&e.client), e.info(CP2102), opts)
            .await
            .unwrap();
        let info = b.info();
        assert_eq!(info.driver, "CP210x");
        assert_eq!((info.line.baud, info.line.data_bits), (115_200, 8));
        let mut c = Rfc::connect(info.tcp_addr.unwrap()).await;
        c.s.write_all(b"tcp side").await.unwrap();
        assert_eq!(c.data(8).await, b"tcp side");
        #[cfg(target_os = "macos")]
        {
            let path = info.pty_path.clone().expect("a PTY path");
            let fd = rustix::fs::open(
                path.as_str(),
                rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            rustix::io::write(&fd, b"pty side").unwrap();
            // The device echoes to both front ends.
            let mut got = Vec::new();
            for _ in 0..400 {
                let mut buf = [0u8; 64];
                if let Ok(n) = rustix::io::read(&fd, &mut buf) {
                    got.extend_from_slice(&buf[..n]);
                }
                if got.len() >= 16 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // The PTY side also sees the echo of what the TCP client wrote first.
            assert_eq!(got, b"tcp sidepty side");
            assert_eq!(c.data(8).await, b"pty side");
        }
        wait_tx(b.port(), 8).await;
        b.stop().await;
        assert_eq!(b.done().await, "port closed");
        // The device is closed on the agent: another command can open it.
        e.client.open_device(CP2102).await.unwrap();
        e.client.close_device(CP2102).await.unwrap();
    }

    #[tokio::test]
    async fn bridge_ends_when_the_device_goes() {
        let e = env().await;
        let opts = BridgeOptions {
            pty: false,
            tcp: Some(LOCAL.parse().unwrap()),
            ..BridgeOptions::default()
        };
        let b = SerialBridge::start(Arc::clone(&e.client), e.info(CDC), opts)
            .await
            .unwrap();
        let rest: Vec<_> = e.infos.iter().filter(|d| d.id != CDC).cloned().collect();
        e.agent.set_devices(rest);
        let reason = tokio::time::timeout(Duration::from_secs(2), b.done()).await.unwrap();
        assert!(reason.contains("device closed"), "{reason}");
        // The TCP port is gone with it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(TcpStream::connect(b.info().tcp_addr.unwrap()).await.is_err());
        b.stop().await;
    }

    #[tokio::test]
    async fn bridge_refuses_what_cannot_be_served() {
        let e = env().await;
        let none = BridgeOptions {
            pty: false,
            tcp: None,
            ..BridgeOptions::default()
        };
        assert!(
            SerialBridge::start(Arc::clone(&e.client), e.info(CP2102), none)
                .await
                .is_err()
        );
        // Not a serial device: nothing is opened.
        let tcp = BridgeOptions {
            pty: false,
            tcp: Some(LOCAL.parse().unwrap()),
            ..BridgeOptions::default()
        };
        let err = SerialBridge::start(Arc::clone(&e.client), e.info(8), tcp)
            .await
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("not a supported USB serial device"));
    }
}

// --- 7.3 / 7.4 measurements --------------------------------------------------------------------

mod bench {
    use hubbridge_host::serial::bench::{EchoOptions, RequestOptions, bench_echo, bench_request};

    use super::*;

    #[tokio::test]
    async fn echo_bench_on_cp210x_and_ft232r() {
        let e = env().await;
        for id in [CP2102, FT232R] {
            let p = e.open(id).await;
            let r = bench_echo(
                &p,
                &EchoOptions {
                    count: 30,
                    bytes: 64 * 1024,
                    ..EchoOptions::default()
                },
            )
            .await
            .unwrap();
            assert_eq!((r.lost, r.wrong), (0, 0), "device {id}");
            assert_eq!(r.rounds, 30);
            let lat = r.latency.expect("percentiles");
            assert!(lat.p50 <= lat.p95 && lat.p95 <= lat.max && lat.max > Duration::ZERO);
            assert_eq!(r.stream_bytes, 64 * 1024);
            assert!(r.kb_per_s > 0.0 && r.line_rate_percent > 0.0);
            p.close().await;
        }
    }

    #[tokio::test]
    async fn echo_bench_with_a_small_window_loses_nothing() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let r = bench_echo(
            &p,
            &EchoOptions {
                count: 2,
                bytes: 1000,
                window: 100,
                size: 8,
                timeout: Duration::from_millis(300),
            },
        )
        .await
        .unwrap();
        assert_eq!((r.lost, r.wrong), (0, 0));
        assert_eq!(r.stream_bytes, 1000);
        p.close().await;
    }

    #[tokio::test]
    async fn request_with_fixed_reply_length() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let r = bench_request(
            &p,
            &RequestOptions {
                request: vec![0x01, 0x02],
                reply_len: Some(2),
                reply_idle: None,
                count: 20,
                interval: Duration::ZERO,
                timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();
        assert_eq!((r.rounds, r.timeouts), (20, 0));
        assert_eq!(r.first_reply, Some(vec![0x01, 0x02]));
        assert!(r.latency.is_some());
        assert_eq!(r.reply_bytes, 40);
        // A reply longer than the device sends: every round times out and is counted.
        let r = bench_request(
            &p,
            &RequestOptions {
                request: vec![0x01, 0x02],
                reply_len: Some(4),
                reply_idle: None,
                count: 3,
                interval: Duration::from_millis(5),
                timeout: Duration::from_millis(150),
            },
        )
        .await
        .unwrap();
        assert_eq!((r.rounds, r.timeouts), (3, 3));
        assert!(r.first_reply.is_none() && r.latency.is_none());
        p.close().await;
    }

    #[tokio::test]
    async fn request_with_idle_end_of_reply() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let r = bench_request(
            &p,
            &RequestOptions {
                request: vec![9, 8, 7],
                reply_len: None,
                reply_idle: Some(Duration::from_millis(40)),
                count: 5,
                interval: Duration::ZERO,
                timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();
        assert_eq!((r.timeouts, r.first_reply), (0, Some(vec![9, 8, 7])));
        assert!(r.reply_bytes_per_s > 0.0);
        p.close().await;
        // Neither rule: refused.
        let p = e.open(CP2102).await;
        assert!(
            bench_request(
                &p,
                &RequestOptions {
                    request: vec![1],
                    reply_len: None,
                    reply_idle: None,
                    count: 1,
                    interval: Duration::ZERO,
                    timeout: Duration::from_secs(1)
                }
            )
            .await
            .is_err()
        );
        p.close().await;
    }
}

// --- review round 1 ----------------------------------------------------------------------------

mod review {
    use hubbridge_host::serial::bridge::{BridgeOptions, SerialBridge};
    use hubbridge_proto::UsbStatus;

    use super::*;

    #[tokio::test]
    async fn close_on_a_half_open_session_is_fast() {
        let e = env_proxied().await;
        let p = e.open(CP2102).await;
        let cdc = e.open(CDC).await;
        // The session goes half open: the phone is still there but nothing gets through.
        e.freeze.store(true, std::sync::atomic::Ordering::SeqCst);
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(8), async {
            p.close().await;
            cdc.close().await;
        })
        .await
        .expect("close does not hang");
        // Each close takes about 2 s at most: 1 s for the chip, then release and close_device.
        assert!(start.elapsed() < Duration::from_secs(6), "{:?}", start.elapsed());
        e.freeze.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    #[tokio::test]
    async fn close_gives_the_device_back_even_when_the_chip_hangs() {
        let e = env().await;
        let p = e.open(CP2102).await;
        // The chip stops answering; the phone and the session are fine.
        e.usb.set_hang_control(CP2102, true);
        let start = std::time::Instant::now();
        p.close().await;
        assert!(start.elapsed() < Duration::from_secs(4), "{:?}", start.elapsed());
        e.usb.set_hang_control(CP2102, false);
        // The device was released and closed on the agent: another session can open it.
        let mut store2 = Store::load(&temp_dir("host2").join("host.json")).unwrap();
        let pin = e.agent.start_pairing();
        let c2 = Client::pair(&[e.addr], &mut store2, &pin).await.unwrap();
        c2.open_device(CP2102).await.expect("the device is free, not Busy");
        c2.close_device(CP2102).await.unwrap();
    }

    #[tokio::test]
    async fn limited_ch34x_uses_the_lower_base_clock() {
        let e = env().await;
        let cfg = SerialConfig {
            line: Line {
                baud: 12_000,
                ..Line::default()
            },
            ..SerialConfig::default()
        };
        let p = e.open_with(CH340_LIMITED, cfg, PortOptions::default()).await;
        // The quirk probe (register 0x05) stalled: the divisor is the LIMITED_PRESCALER one.
        let reqs = requests(&e.usb, CH340_LIMITED);
        assert!(reqs.contains(&(0xc0, 0x95, 0x0005, 0)));
        assert!(reqs.contains(&(0x40, 0x9a, 0x1312, 0xc182)), "{reqs:?}");
        // No break on such a chip.
        assert!(p.set_break(true).await.is_err());
        p.close().await;
        // The same rate on a chip without the quirk.
        let p = e.open_with(CH340, cfg, PortOptions::default()).await;
        assert!(requests(&e.usb, CH340).contains(&(0x40, 0x9a, 0x1312, 0x8386)));
        p.close().await;
    }

    #[tokio::test]
    async fn ch34x_open_fails_on_an_unexpected_probe_status_or_a_short_read() {
        let e = env().await;
        // Any status but OK or STALL from the quirk probe fails the open, as the Linux ch341 driver does.
        e.usb.fail_control_request(CH340, Some((0x95, UsbStatus::Fault)));
        let err = SerialPort::open(
            Arc::clone(&e.client),
            e.info(CH340),
            SerialConfig::default(),
            PortOptions::default(),
        )
        .await
        .err()
        .expect("open fails");
        assert!(format!("{err:#}").contains("quirk probe"), "{err:#}");
        e.usb.fail_control_request(CH340, None);
        // A one-byte version is a short read.
        e.usb.set_short_reads(CH340, true);
        let err = SerialPort::open(
            Arc::clone(&e.client),
            e.info(CH340),
            SerialConfig::default(),
            PortOptions::default(),
        )
        .await
        .err()
        .expect("open fails");
        assert!(format!("{err:#}").contains("short answer"), "{err:#}");
        e.usb.set_short_reads(CH340, false);
        // The quirk probe answering OK with fewer than 2 bytes fails too.
        e.usb.set_short_value(CH340, Some(0x0005));
        let err = SerialPort::open(
            Arc::clone(&e.client),
            e.info(CH340),
            SerialConfig::default(),
            PortOptions::default(),
        )
        .await
        .err()
        .expect("open fails");
        assert!(format!("{err:#}").contains("quirk probe: short answer"), "{err:#}");
        e.usb.set_short_value(CH340, None);
        // All failures gave the device back.
        e.client.open_device(CH340).await.unwrap();
        e.client.close_device(CH340).await.unwrap();
        e.open(CH340).await.close().await;
    }

    #[tokio::test]
    async fn failed_out_transfer_is_counted_and_reported() {
        let e = env().await;
        let p = e.open(CP2102).await;
        let mut events = p.subscribe();
        e.usb.fail_next_out(CP2102, UsbStatus::Fault);
        p.write(&pattern(100)).await.unwrap();
        p.drain().await;
        loop {
            match tokio::time::timeout(Duration::from_secs(3), events.recv()).await {
                Ok(Ok(PortEvent::OutFailed { bytes, status })) => {
                    assert_eq!((bytes, status), (100, UsbStatus::Fault));
                    break;
                }
                Ok(Ok(_)) => {}
                other => panic!("no OutFailed event: {other:?}"),
            }
        }
        let s = p.stats();
        assert_eq!((s.tx, s.tx_failed), (0, 100));
        // One failure does not end the port; the next write works.
        p.write(b"fine").await.unwrap();
        assert_eq!(read_exact(&p, 4).await, b"fine");
        wait_tx(&p, 4).await;
        assert_eq!(p.stats().tx, 4);
        assert!(p.is_done().is_none());
        p.close().await;
    }

    #[tokio::test]
    async fn endpoint_that_stalls_forever_stops_the_port() {
        let e = env().await;
        let p = e.open(CP2102).await;
        e.usb.set_stall_in(CP2102, true);
        let reason = tokio::time::timeout(Duration::from_secs(5), p.done())
            .await
            .expect("the port gives up");
        assert!(reason.contains("stalling"), "{reason}");
        e.usb.set_stall_in(CP2102, false);
        p.close().await;
    }

    #[tokio::test]
    async fn cancelled_in_reply_with_data_is_delivered() {
        let e = env().await;
        let p = e.open(CP2102).await;
        e.usb.cancel_next_in_with(CP2102, b"partial");
        assert_eq!(read_exact(&p, 7).await, b"partial");
        assert!(p.is_done().is_none(), "a cancelled transfer does not stop the port");
        // And it keeps reading afterwards.
        e.usb.inject_rx(CP2102, b"more");
        assert_eq!(read_exact(&p, 4).await, b"more");
        p.close().await;
    }

    #[tokio::test]
    async fn bridge_ends_when_a_front_end_ends() {
        let e = env().await;
        let opts = BridgeOptions {
            pty: false,
            tcp: Some("127.0.0.1:0".parse().unwrap()),
            ..BridgeOptions::default()
        };
        let b = SerialBridge::start(Arc::clone(&e.client), e.info(CP2102), opts)
            .await
            .unwrap();
        b.stop_fronts();
        let reason = tokio::time::timeout(Duration::from_secs(3), b.done())
            .await
            .expect("the bridge ends with its front end");
        assert!(reason.contains("front end"), "{reason}");
        // The supervisor closed the port: the device is free again.
        let mut ok = false;
        for _ in 0..100 {
            if e.client.open_device(CP2102).await.is_ok() {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ok, "the device was given back");
        e.client.close_device(CP2102).await.unwrap();
    }

    #[tokio::test]
    async fn invalid_data_bits_are_refused_before_opening() {
        let e = env().await;
        let cfg = SerialConfig {
            line: Line {
                data_bits: 9,
                ..Line::default()
            },
            ..SerialConfig::default()
        };
        let err = SerialPort::open(Arc::clone(&e.client), e.info(CP2102), cfg, PortOptions::default())
            .await
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("5 to 8"), "{err:#}");
        // The device was never opened.
        e.client.open_device(CP2102).await.unwrap();
        e.client.close_device(CP2102).await.unwrap();
        let p = e.open(CP2102).await;
        assert!(
            p.set_line(Line {
                data_bits: 4,
                ..Line::default()
            })
            .await
            .is_err()
        );
        assert_eq!(p.line().data_bits, 8);
        p.close().await;
    }

    #[tokio::test]
    async fn ftdi_close_turns_flow_control_off_first() {
        let e = env().await;
        let p = e.open(FT232R).await;
        e.usb.clear_serial_log(FT232R);
        p.close().await;
        let reqs = requests(&e.usb, FT232R);
        assert_eq!(
            reqs.first(),
            Some(&(0x40, 0x02, 0, 0)),
            "flow control off first: {reqs:?}"
        );
        assert!(reqs.contains(&(0x40, 0x01, 0x0100, 0)) && reqs.contains(&(0x40, 0x01, 0x0200, 0)));
    }
}

/// Closing a port while the host's socket is full: its close budgets run out and its tasks
/// are aborted in the middle of requests. The session must survive (GitHub issue #7).
#[tokio::test]
async fn serial_close_budgets_keep_session() {
    let e = env().await;
    let proxy = PausableProxy::start(e.addr).await;
    let mut store = Store::load(&e.store_path).unwrap();
    let client = Arc::new(Client::connect(&[proxy.addr()], &mut store).await.unwrap());
    let mut events = client.take_events().unwrap();
    let port = Arc::new(
        SerialPort::open(
            Arc::clone(&client),
            e.info(CP2102),
            SerialConfig::default(),
            PortOptions::default(),
        )
        .await
        .unwrap(),
    );

    // Some data goes through first, then the agent side stops reading and the host's socket
    // is filled with big requests for a device that is not open (kept, so their answers can
    // be checked); the last one is dropped by its timeout.
    port.write(&pattern(1000)).await.unwrap();
    wait_tx(&port, 1000).await;
    proxy.pause();
    let mut fillers = Vec::new();
    let mut expired = false;
    for _ in 0..400 {
        let msg = Message::Transfer {
            id: 999,
            endpoint: 0x01,
            length: 0,
            data: vec![0x5A; 64 * 1024],
            timeout_ms: 1000,
        };
        match tokio::time::timeout(Duration::from_millis(200), client.submit(msg)).await {
            Ok(p) => fillers.push(p.unwrap()),
            Err(_) => {
                expired = true;
                break;
            }
        }
    }
    assert!(expired, "the socket never filled up");

    // The port still has OUT data to send; closing it puts its control, release and close
    // requests behind the full queue, so every budget runs out and its tasks are aborted.
    let writer = {
        let port = Arc::clone(&port);
        tokio::spawn(async move {
            for _ in 0..8 {
                if port.write(&pattern(64 * 1024)).await.is_err() {
                    return;
                }
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let closing = tokio::time::timeout(Duration::from_secs(5), port.close()).await;
    assert!(closing.is_ok(), "close must stay within its budgets");
    writer.abort();
    let _ = writer.await;
    proxy.resume();

    for p in fillers {
        let e = p.wait(Duration::from_secs(60)).await.unwrap_err();
        assert!(
            matches!(e.downcast_ref::<AgentError>(), Some(a) if a.code == ErrorCode::NotOpen),
            "{e:#}"
        );
    }
    client.ping(1).await.unwrap();
    assert!(client.list_devices().await.unwrap().iter().any(|d| d.id == CP2102));
    // The device can be opened again on the same session, and a port opened after closing
    // it echoes data.
    client.open_device(CP2102).await.unwrap();
    client.close_device(CP2102).await.unwrap();
    let port = SerialPort::open(
        Arc::clone(&client),
        e.info(CP2102),
        SerialConfig::default(),
        PortOptions::default(),
    )
    .await
    .unwrap();
    port.write(b"hello after the storm").await.unwrap();
    assert_eq!(read_exact(&port, 21).await, b"hello after the storm");
    port.close().await;
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(500), events.recv()).await,
            Ok(Some(hubbridge_host::ClientEvent::Disconnected(_)))
        ),
        "no Disconnected event expected"
    );
}
