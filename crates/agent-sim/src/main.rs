//! `hubbridge-agent-sim` — the agent core with fake USB devices, on a desktop.
//!
//! Lets you develop and test the host without a phone:
//!
//! ```text
//! hubbridge-agent-sim --pin 123456          # terminal 1
//! hubbridge discover                         # terminal 2
//! hubbridge pair 127.0.0.1 --pin 123456
//! hubbridge devices
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use hubbridge_agent::{Agent, AgentCallbacks, AgentConfig};
use hubbridge_proto::{DEFAULT_PORT, MDNS_SERVICE_TYPE, PROTO_VERSION, fixtures};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tracing::info;

#[derive(Parser)]
#[command(
    name = "hubbridge-agent-sim",
    version,
    about = "HubBridge agent simulator with fake USB devices"
)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)))]
    listen: SocketAddr,
    /// Agent name shown to hosts.
    #[arg(long, default_value = "HubBridge Simulator")]
    name: String,
    /// Where to keep the agent id and paired hosts.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Use this PIN instead of a random one (handy in scripts).
    #[arg(long)]
    pin: Option<String>,
    /// Every N seconds plug/unplug a fake HID keyboard, to test hot-plug events.
    #[arg(long)]
    churn: Option<u64>,
    /// Do not announce on mDNS.
    #[arg(long)]
    no_mdns: bool,
}

struct Log;

impl AgentCallbacks for Log {
    fn permission_requested(&self, id: u32) {
        info!(
            device = id,
            "host asked for USB permission (a phone would show the system dialog)"
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("HUBBRIDGE_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let state_dir = args
        .state_dir
        .unwrap_or_else(|| std::env::temp_dir().join("hubbridge-agent-sim"));
    let agent = Agent::new(AgentConfig {
        state_dir: state_dir.clone(),
        name: args.name.clone(),
        model: "simulator".into(),
        os: format!("{} (simulated)", std::env::consts::OS),
        app_version: env!("CARGO_PKG_VERSION").into(),
        usb_host: true,
    })?;
    agent.set_callbacks(std::sync::Arc::new(Log));

    let uart = fixtures::cp2102_uart(1002);
    let keyboard = fixtures::without_permission(fixtures::hid_keyboard(1003));
    agent.set_devices(vec![uart.clone(), keyboard.clone()]);

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    let port = listener.local_addr()?.port();

    let pin = match args.pin {
        Some(p) => agent.start_pairing_with_pin(p),
        None => agent.start_pairing(),
    };
    println!("agent id   {}", agent.agent_id());
    println!("listening  {}", listener.local_addr()?);
    println!("state      {}", state_dir.display());
    println!(
        "PIN        {pin}   (valid {} s; restart for a new one)",
        hubbridge_agent::PAIRING_TTL.as_secs()
    );

    let _mdns = if args.no_mdns {
        None
    } else {
        Some(announce(&agent, port, args.listen.is_ipv6())?)
    };

    if let Some(secs) = args.churn {
        let agent = agent.clone();
        tokio::spawn(async move {
            let mut present = true;
            loop {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                present = !present;
                let mut list = vec![uart.clone()];
                if present {
                    list.push(keyboard.clone());
                }
                info!(keyboard = present, "churn: device list changed");
                agent.set_devices(list);
            }
        });
    }

    let serve = agent.clone();
    tokio::select! {
        res = serve.serve(listener) => res?,
        _ = tokio::signal::ctrl_c() => { agent.shutdown(); }
    }
    Ok(())
}

fn announce(agent: &Agent, port: u16, ipv6: bool) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    // The simulator listens on IPv4 by default: do not advertise IPv6 addresses.
    if !ipv6 {
        daemon.disable_interface(mdns_sd::IfKind::IPv6)?;
    }
    let id = agent.agent_id();
    let instance = format!("{}-{}", agent.name().replace(' ', "-"), &id[..6]);
    let props = [
        ("id", id.as_str()),
        ("v", &PROTO_VERSION.to_string()),
        ("name", agent.name()),
    ];
    let info = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        &instance,
        "hubbridge-sim.local.",
        "",
        port,
        &props[..],
    )?
    .enable_addr_auto();
    daemon.register(info)?;
    Ok(daemon)
}
