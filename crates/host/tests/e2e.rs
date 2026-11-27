//! End to end over real TCP on localhost: agent core ↔ host client.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use hubbridge_agent::{Agent, AgentCallbacks, AgentConfig, PAIRING_MAX_ATTEMPTS};
use hubbridge_host::{Client, ClientEvent, Store};
use hubbridge_proto::{Message, fixtures};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hubbridge-test-{tag}-{}", hubbridge_link::random_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[derive(Default)]
struct Calls {
    permission: AtomicU32,
}

impl AgentCallbacks for Calls {
    fn permission_requested(&self, id: u32) {
        self.permission.store(id, Ordering::SeqCst);
    }
}

async fn start_agent() -> (Agent, std::net::SocketAddr, Arc<Calls>) {
    let agent = Agent::new(AgentConfig {
        state_dir: temp_dir("agent"),
        name: "Test Phone".into(),
        model: "test".into(),
        os: "test".into(),
        app_version: "0".into(),
        usb_host: true,
    })
    .unwrap();
    let calls = Arc::new(Calls::default());
    agent.set_callbacks(calls.clone());
    agent.set_devices(vec![fixtures::cp2102_uart(10)]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let a = agent.clone();
    tokio::spawn(async move { a.serve(listener).await });
    (agent, addr, calls)
}

/// The agent's session list empties shortly after the host closed (orderly close).
async fn wait_no_sessions(agent: &Agent) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !agent.status().sessions.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the agent still lists the session 2 s after close()");
}

#[tokio::test]
async fn pair_list_ping_events() {
    let (agent, addr, calls) = start_agent().await;
    let mut store = Store::load(&temp_dir("host").join("host.json")).unwrap();

    // Resume before pairing is refused.
    let err = Client::connect(&[addr], &mut store).await.err().unwrap();
    assert!(format!("{err:#}").contains("pair first"), "{err:#}");

    // Pairing without an open window is refused.
    let err = Client::pair(&[addr], &mut store, "123456").await.err().unwrap();
    assert!(format!("{err:#}").contains("PairingNotActive"), "{err:#}");

    let pin = agent.start_pairing();
    let c = Client::pair(&[addr], &mut store, &pin).await.unwrap();
    assert_eq!(c.agent_id, agent.agent_id());
    c.close().await;
    wait_no_sessions(&agent).await;
    assert!(store.key_for(&agent.agent_id()).is_some());
    assert_eq!(agent.status().paired_hosts.len(), 1);
    assert!(!agent.status().pairing_active, "the PIN is single-use");

    // The key survives a reload of the store.
    let mut store = Store::load(store.path()).unwrap();
    let c = Client::connect(&[addr], &mut store).await.unwrap();
    let info = c.agent_info().await.unwrap();
    assert_eq!(info.name, "Test Phone");
    let devices = c.list_devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].vendor_id, 0x10c4);
    assert_eq!(devices[0].configurations[0].interfaces[0].endpoints.len(), 2);
    let rtt = c.ping(1).await.unwrap();
    assert!(rtt < Duration::from_secs(1));

    c.request_permission(10).await.unwrap();
    assert_eq!(calls.permission.load(Ordering::SeqCst), 10);
    assert!(c.request_permission(99).await.is_err());

    // Hot-plug events.
    let mut events = c.take_events().unwrap();
    agent.set_devices(vec![fixtures::cp2102_uart(10), fixtures::hid_keyboard(11)]);
    match tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
    {
        Some(ClientEvent::Message(Message::DeviceAdded(d))) => assert_eq!(d.id, 11),
        other => panic!("unexpected {other:?}"),
    }
    agent.set_devices(vec![fixtures::hid_keyboard(11)]);
    match tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
    {
        Some(ClientEvent::Message(Message::DeviceRemoved { id })) => assert_eq!(id, 10),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(agent.status().sessions.len(), 1);

    // Agent shutdown reaches the host.
    agent.shutdown();
    match tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
    {
        Some(ClientEvent::Disconnected(_)) => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn wrong_pins_lock_pairing() {
    let (agent, addr, _) = start_agent().await;
    let mut store = Store::load(&temp_dir("host").join("host.json")).unwrap();
    let pin = agent.start_pairing();
    let wrong = if pin == "000000" { "111111" } else { "000000" };
    for _ in 0..PAIRING_MAX_ATTEMPTS {
        let err = Client::pair(&[addr], &mut store, wrong).await.err().unwrap();
        assert!(format!("{err:#}").contains("wrong PIN"), "{err:#}");
    }
    // Even the right PIN is refused now: a new one must be started on the phone.
    let err = Client::pair(&[addr], &mut store, &pin).await.err().unwrap();
    assert!(format!("{err:#}").contains("PairingLocked"), "{err:#}");
    assert!(store.data.agents.is_empty());

    let pin = agent.start_pairing();
    Client::pair(&[addr], &mut store, &pin).await.unwrap().close().await;
    assert_eq!(store.data.agents.len(), 1);
}

#[tokio::test]
async fn close_after_pipelined_requests() {
    let (agent, addr, _calls) = start_agent().await;
    let mut store = Store::load(&temp_dir("host").join("host.json")).unwrap();
    let pin = agent.start_pairing();
    Client::pair(&[addr], &mut store, &pin).await.unwrap().close().await;
    wait_no_sessions(&agent).await;

    let c = Client::connect(&[addr], &mut store).await.unwrap();
    assert_eq!(agent.status().sessions.len(), 1);
    let mut pendings = Vec::new();
    for seq in 0..8 {
        pendings.push(c.submit(Message::Ping { seq, sent_us: 0 }).await.unwrap());
    }
    c.close().await;
    // The agent ends the session as an orderly close after the pipelined requests.
    wait_no_sessions(&agent).await;

    // The key store still works: a new session can be resumed.
    let c = Client::connect(&[addr], &mut store).await.unwrap();
    c.ping(1).await.unwrap();
    c.close().await;
    wait_no_sessions(&agent).await;
}
