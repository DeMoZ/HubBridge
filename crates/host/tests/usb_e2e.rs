//! Step 2 end to end over real TCP: host client ↔ agent core ↔ fake USB devices.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::PausableProxy;
use hubbridge_agent::fake::{FakeKind, FakeUsb, block_byte};
use hubbridge_agent::{Agent, AgentConfig};
use hubbridge_host::client::AgentError;
use hubbridge_host::msc::{self, MassStorage};
use hubbridge_host::{Client, ClientEvent, Store};
use hubbridge_proto::{ErrorCode, Message, Setup, UsbStatus, fixtures};

const T: Duration = Duration::from_secs(2);

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hubbridge-usb-{tag}-{}", hubbridge_link::random_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn setup() -> (Agent, std::net::SocketAddr, Store) {
    let agent = Agent::new(AgentConfig {
        state_dir: temp_dir("agent"),
        name: "USB Phone".into(),
        model: "test".into(),
        os: "test".into(),
        app_version: "0".into(),
        usb_host: true,
    })
    .unwrap();
    agent.set_devices(vec![
        fixtures::cp2102_uart(1),
        fixtures::without_permission(fixtures::hid_keyboard(2)),
        fixtures::flash_drive(3),
        fixtures::cp2102_uart(4),
    ]);
    let usb = FakeUsb::new();
    usb.add(
        1,
        fixtures::cp2102_raw(),
        &["Silicon Labs", "CP2102", "0001"],
        FakeKind::Loopback,
        true,
    );
    usb.add(
        2,
        fixtures::hid_keyboard_raw(),
        &["Logitech", "USB Keyboard"],
        FakeKind::DescriptorsOnly,
        false,
    );
    usb.add(
        3,
        fixtures::flash_drive_raw(),
        &["Generic", "Mass Storage Device", "SN"],
        FakeKind::MassStorage { blocks: 4096 },
        true,
    );
    usb.add(4, fixtures::cp2102_raw(), &[], FakeKind::PanicOnOpen, true);
    agent.set_usb_backend(usb);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let a = agent.clone();
    tokio::spawn(async move { a.serve(listener).await });
    let mut store = Store::load(&temp_dir("host").join("host.json")).unwrap();
    let pin = agent.start_pairing();
    Client::pair(&[addr], &mut store, &pin).await.unwrap().close().await;
    (agent, addr, store)
}

fn agent_code(e: &anyhow::Error) -> Option<ErrorCode> {
    e.downcast_ref::<AgentError>().map(|a| a.code)
}

#[tokio::test]
async fn descriptors_loopback_and_errors() {
    let (agent, addr, mut store) = setup().await;
    let c = Client::connect(&[addr], &mut store).await.unwrap();

    // Not open yet.
    let e = c
        .control(1, Setup::get_descriptor(1, 0, 0, 18), vec![], T)
        .await
        .unwrap_err();
    assert_eq!(agent_code(&e), Some(ErrorCode::NotOpen));
    // No permission on the phone.
    let e = c.open_device(2).await.unwrap_err();
    assert_eq!(agent_code(&e), Some(ErrorCode::NoPermission));
    // Unknown device.
    assert_eq!(
        agent_code(&c.open_device(99).await.unwrap_err()),
        Some(ErrorCode::UnknownDevice)
    );

    c.open_device(1).await.unwrap();
    let (raw, strings) = msc::read_descriptors(&c, 1).await.unwrap();
    assert_eq!(raw, fixtures::cp2102_raw());
    assert!(strings.iter().any(|(_, s)| s == "Silicon Labs"));

    // Bulk needs a claimed interface.
    let r = c.transfer(1, 0x01, 0, b"x".to_vec(), T).await.unwrap();
    assert_eq!(r.status, UsbStatus::InvalidArgument);
    c.claim_interface(1, 0, false).await.unwrap();

    // Loopback: several writes in flight, then reads, all in order.
    let mut writes = Vec::new();
    for i in 0..10u8 {
        writes.push(c.submit_transfer(1, 0x01, 0, vec![i; 100], T).await.unwrap());
    }
    for w in writes {
        let r = w.wait().await.unwrap();
        assert_eq!((r.status, r.actual), (UsbStatus::Ok, 100));
    }
    let mut got = Vec::new();
    while got.len() < 1000 {
        got.extend(c.transfer(1, 0x81, 256, vec![], T).await.unwrap().ok().unwrap());
    }
    let want: Vec<u8> = (0..10u8).flat_map(|i| vec![i; 100]).collect();
    assert_eq!(got, want);
    // Nothing more to read: the IN times out.
    let r = c
        .transfer(1, 0x81, 64, vec![], Duration::from_millis(100))
        .await
        .unwrap();
    assert_eq!(r.status, UsbStatus::Cancelled);

    // A second computer cannot take the open device.
    let mut store2 = Store::load(&temp_dir("host2").join("host.json")).unwrap();
    let pin = agent.start_pairing();
    let c2 = Client::pair(&[addr], &mut store2, &pin).await.unwrap();
    assert_eq!(agent_code(&c2.open_device(1).await.unwrap_err()), Some(ErrorCode::Busy));

    // Unplug: the owner gets DeviceClosed and the device is free again.
    let mut events = c.take_events().unwrap();
    agent.set_devices(vec![fixtures::flash_drive(3)]);
    let mut closed = false;
    for _ in 0..4 {
        if let Some(ClientEvent::Message(Message::DeviceClosed { id: 1, .. })) =
            tokio::time::timeout(T, events.recv()).await.unwrap()
        {
            closed = true;
            break;
        }
    }
    assert!(closed);
    assert_eq!(
        agent_code(&c.close_device(1).await.unwrap_err()),
        Some(ErrorCode::NotOpen)
    );
}

#[tokio::test]
async fn mass_storage_read() {
    let (_agent, addr, mut store) = setup().await;
    let c = Client::connect(&[addr], &mut store).await.unwrap();
    let info = c.list_devices().await.unwrap().into_iter().find(|d| d.id == 3).unwrap();
    assert_eq!(msc::find_interface(&info), Some((0, 0x81, 0x02)));

    let mut ms = MassStorage::open(&c, &info).await.unwrap();
    let inq = ms.inquiry().await.unwrap();
    assert_eq!(
        (inq.vendor.as_str(), inq.product.as_str(), inq.removable),
        ("Generic", "Flash Disk", true)
    );
    let cap = ms.read_capacity().await.unwrap();
    assert_eq!((cap.blocks, cap.block_size), (4096, 512));

    // One command larger than a chunk: several transfers in flight, data in order.
    let data = ms.read(100, 300, 512).await.unwrap();
    assert_eq!(data.len(), 300 * 512);
    for (i, b) in data.iter().enumerate() {
        assert_eq!(*b, block_byte(100 + (i / 512) as u32, i % 512), "byte {i}");
    }
    // Past the end: the drive fails the command, the next one still works.
    assert!(ms.read(4095, 2, 512).await.is_err());
    let r = ms.bench(cap, 2 * 1024 * 1024, None).await.unwrap();
    assert_eq!(r.bytes, 2 * 1024 * 1024);
    ms.close().await.unwrap();
    // Closed: it can be opened again.
    c.open_device(3).await.unwrap();
}

#[tokio::test]
async fn spec_edge_cases() {
    let (_agent, addr, mut store) = setup().await;
    let c = Client::connect(&[addr], &mut store).await.unwrap();

    // 5.2: a panic inside the agent still answers, with Internal; the session survives.
    let e = c.open_device(4).await.unwrap_err();
    assert_eq!(agent_code(&e), Some(ErrorCode::Internal), "{e:#}");
    c.ping(1).await.unwrap();

    // 6.3: not a drive: refused before the device is opened, so it can be opened after.
    let uart = c.list_devices().await.unwrap().into_iter().find(|d| d.id == 1).unwrap();
    let e = MassStorage::open(&c, &uart).await.err().unwrap();
    assert!(format!("{e:#}").contains("not a USB mass-storage device"), "{e:#}");
    c.open_device(1).await.unwrap();
    c.claim_interface(1, 0, false).await.unwrap();

    // 6.1: more than 1 MiB in one transfer.
    let e = c.transfer(1, 0x81, 2 * 1024 * 1024, vec![], T).await.unwrap_err();
    assert_eq!(agent_code(&e), Some(ErrorCode::Unsupported));

    // 6.2: more requests in flight than the 256 the agent keeps pending: none dropped.
    let mut pending = Vec::new();
    for _ in 0..300 {
        pending.push(
            c.submit_transfer(1, 0x81, 64, vec![], Duration::from_millis(300))
                .await
                .unwrap(),
        );
    }
    let mut answered = 0;
    for p in pending {
        assert_eq!(p.wait().await.unwrap().status, UsbStatus::Cancelled);
        answered += 1;
    }
    assert_eq!(answered, 300);

    // 6.4: recovery requests.
    c.clear_halt(1, 0x81).await.unwrap();
    c.reset_device(1).await.unwrap();
    c.close_device(1).await.unwrap();
}

/// Requests that are dropped while the host's socket is full must not damage the session
/// (GitHub issue #7): the agent decrypts every frame and the session stays open.
#[tokio::test]
async fn session_survives_dropped_submits_on_full_socket() {
    let (agent, addr, mut store) = setup().await;
    let proxy = PausableProxy::start(addr).await;
    let c = std::sync::Arc::new(Client::connect(&[proxy.addr()], &mut store).await.unwrap());
    let mut events = c.take_events().unwrap();
    c.ping(1).await.unwrap();
    assert_eq!(agent.status().sessions.len(), 1);

    proxy.pause();
    let mut accepted = Vec::new();
    let mut expired = false;
    for _ in 0..400 {
        let msg = Message::Transfer {
            id: 999,
            endpoint: 0x01,
            length: 0,
            data: vec![0x5A; 64 * 1024],
            timeout_ms: 1000,
        };
        match tokio::time::timeout(Duration::from_millis(200), c.submit(msg)).await {
            Ok(p) => accepted.push(p.unwrap()),
            Err(_) => {
                expired = true;
                break;
            }
        }
    }
    assert!(expired, "the socket never filled up");
    // Another caller blocked in submit is aborted.
    let c2 = c.clone();
    let blocked = tokio::spawn(async move {
        let _ = c2
            .submit(Message::Transfer {
                id: 999,
                endpoint: 0x01,
                length: 0,
                data: vec![0x5A; 64 * 1024],
                timeout_ms: 1000,
            })
            .await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    blocked.abort();
    assert!(blocked.await.unwrap_err().is_cancelled());

    proxy.resume();
    for p in accepted {
        let e = p.wait(Duration::from_secs(30)).await.unwrap_err();
        assert_eq!(agent_code(&e), Some(ErrorCode::NotOpen), "{e:#}");
    }
    c.ping(2).await.unwrap();
    assert!(!c.list_devices().await.unwrap().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .is_err(),
        "no Disconnected event expected"
    );
    assert_eq!(agent.status().sessions.len(), 1);
}
