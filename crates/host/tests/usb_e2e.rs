//! Step 2 end to end over real TCP: host client ↔ agent core ↔ fake USB devices.

use std::path::PathBuf;
use std::time::Duration;

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
    let r = ms.bench(cap, 2 * 1024 * 1024).await.unwrap();
    assert_eq!(r.bytes, 2 * 1024 * 1024);
    ms.close().await.unwrap();
    // Closed: it can be opened again.
    c.open_device(3).await.unwrap();
}
