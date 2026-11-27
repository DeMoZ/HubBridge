//! `hubbridge` — desktop CLI for the HubBridge agent.

use std::io::Write;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use hubbridge_host::{Client, ClientEvent, Store, discovery, format, resolve};
use hubbridge_proto::Message;

#[derive(Parser)]
#[command(
    name = "hubbridge",
    version,
    about = "Use the USB devices of an Android phone's hub from this computer"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List agents announced on the local network (mDNS / Android NSD).
    Discover {
        /// Seconds to listen.
        #[arg(long, default_value_t = 3)]
        timeout: u64,
    },
    /// Pair with an agent using the PIN it shows (Pair button in the app).
    Pair {
        /// `adb`, `adb:<serial>`, `<ip>[:port]`, or an agent name / id.
        target: String,
        /// 6-digit PIN; asked for if not given.
        #[arg(long)]
        pin: Option<String>,
    },
    /// Show the agent's model, OS, app and protocol versions.
    Info { target: Option<String> },
    /// List the USB devices attached to the phone, with descriptors.
    Devices {
        target: Option<String>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Ask the phone to show its USB permission dialog for a device.
    Grant {
        /// Device id from `hubbridge devices`.
        id: u32,
        target: Option<String>,
    },
    /// Measure round-trip time through the encrypted session.
    Ping {
        target: Option<String>,
        #[arg(short, long, default_value_t = 10)]
        count: u32,
        /// Milliseconds between pings.
        #[arg(short, long, default_value_t = 200)]
        interval: u64,
    },
    /// Stay connected: print device changes and RTT, reconnect when the link drops.
    Watch { target: Option<String> },
    /// Set up `adb forward` to the agent and print the local address.
    Adb {
        #[arg(long)]
        serial: Option<String>,
    },
    /// List paired agents.
    Agents,
    /// Forget a paired agent (also remove this computer in the phone app).
    Forget { agent: String },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("HUBBRIDGE_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn connect(target: Option<&str>, store: &mut Store) -> Result<Client> {
    let addrs = resolve(target, store).await?;
    Client::connect(&addrs, store).await
}

async fn run(cli: Cli) -> Result<()> {
    let mut store = Store::load_default()?;
    match cli.cmd {
        Cmd::Discover { timeout } => {
            let found = discovery::discover(Duration::from_secs(timeout)).await?;
            if found.is_empty() {
                println!(
                    "No agents found. Is the app running on the same network? (Some Wi-Fi networks block mDNS: use the IP.)"
                );
            }
            for a in found {
                let paired = if store.key_for(&a.agent_id).is_some() {
                    "paired"
                } else {
                    "not paired"
                };
                let addrs: Vec<String> = a.addrs.iter().map(|x| x.to_string()).collect();
                println!(
                    "{}  id {}  {}  [{}]",
                    a.name,
                    short(&a.agent_id),
                    addrs.join(", "),
                    paired
                );
            }
        }
        Cmd::Pair { target, pin } => {
            let pin = match pin {
                Some(p) => p,
                None => prompt("PIN shown on the phone: ")?,
            };
            if pin.len() != 6 || !pin.chars().all(|c| c.is_ascii_digit()) {
                bail!("the PIN is 6 digits");
            }
            let addrs = resolve(Some(&target), &store).await?;
            let client = Client::pair(&addrs, &mut store, &pin).await?;
            println!(
                "Paired with {} (id {}) at {}",
                client.agent_name,
                short(&client.agent_id),
                client.addr
            );
            println!("Saved in {}", store.path().display());
            client.close().await;
        }
        Cmd::Info { target } => {
            let c = connect(target.as_deref(), &mut store).await?;
            let info = c.agent_info().await?;
            println!("{}\n  address  {}", format::agent_info(&info), c.addr);
            c.close().await;
        }
        Cmd::Devices { target, json } => {
            let c = connect(target.as_deref(), &mut store).await?;
            let devices = c.list_devices().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else if devices.is_empty() {
                println!("No USB devices on {}.", c.agent_name);
            } else {
                for d in &devices {
                    print!("{}", format::device(d));
                }
                if devices.iter().any(|d| !d.has_permission) {
                    println!(
                        "\nDevices without permission show limited details: `hubbridge grant <id>` asks the phone."
                    );
                }
            }
            c.close().await;
        }
        Cmd::Grant { id, target } => {
            let c = connect(target.as_deref(), &mut store).await?;
            c.request_permission(id).await?;
            println!("The phone shows the permission dialog for device {id}: tap Allow there.");
            c.close().await;
        }
        Cmd::Ping {
            target,
            count,
            interval,
        } => {
            let c = connect(target.as_deref(), &mut store).await?;
            println!("Ping {} at {}", c.agent_name, c.addr);
            let mut samples = Vec::new();
            for seq in 1..=count {
                match c.ping(seq).await {
                    Ok(rtt) => {
                        println!("  seq {seq}: {}", format::ms(rtt));
                        samples.push(rtt);
                    }
                    Err(e) => println!("  seq {seq}: {e}"),
                }
                if seq < count {
                    tokio::time::sleep(Duration::from_millis(interval)).await;
                }
            }
            println!("{}", format::rtt_summary(&samples));
            c.close().await;
        }
        Cmd::Watch { target } => watch(target, &mut store).await?,
        Cmd::Adb { serial } => {
            let addr = hubbridge_host::adb::forward(serial.as_deref(), hubbridge_proto::DEFAULT_PORT).await?;
            println!("{addr}");
        }
        Cmd::Agents => {
            if store.data.agents.is_empty() {
                println!("No paired agents. Host id {}.", short(&store.data.host_id));
            }
            for (id, a) in &store.data.agents {
                println!(
                    "{}  id {}  last address {}",
                    a.name,
                    short(id),
                    a.last_addr.as_deref().unwrap_or("-")
                );
            }
        }
        Cmd::Forget { agent } => {
            let id = match store.find(&agent)? {
                Some((id, _)) => id.clone(),
                None => bail!("no paired agent '{agent}'"),
            };
            store.data.agents.remove(&id);
            store.save()?;
            println!("Forgot {}. Remove this computer in the phone app too.", short(&id));
        }
    }
    Ok(())
}

async fn watch(target: Option<String>, store: &mut Store) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        let client = match connect(target.as_deref(), store).await {
            Ok(c) => c,
            Err(e) => {
                // Pairing problems will not fix themselves.
                let text = format!("{e:#}");
                if text.contains("pair") {
                    return Err(e);
                }
                eprintln!("connect failed: {text}; retry in {}s", backoff.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        println!("connected to {} at {}", client.agent_name, client.addr);
        let mut events = client.take_events().expect("fresh client");
        for d in client.list_devices().await.unwrap_or_default() {
            println!("  present  {}", d.label());
        }
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut seq = 0u32;
        let reason = loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { client.close().await; return Ok(()); }
                _ = tick.tick() => {
                    seq += 1;
                    match client.ping(seq).await {
                        Ok(rtt) => eprintln!("  rtt {}", format::ms(rtt)),
                        Err(e) => break format!("ping failed: {e}"),
                    }
                }
                ev = events.recv() => match ev {
                    Some(ClientEvent::Message(Message::DeviceAdded(d))) => println!("  added    {}", d.label()),
                    Some(ClientEvent::Message(Message::DeviceRemoved { id })) => println!("  removed  [{id}]"),
                    Some(ClientEvent::Message(Message::DeviceList { devices })) => println!("  resync: {} devices", devices.len()),
                    Some(ClientEvent::Message(other)) => println!("  event    {other:?}"),
                    Some(ClientEvent::Disconnected(r)) => break r,
                    None => break "event stream ended".into(),
                },
            }
        };
        eprintln!("disconnected: {reason}; reconnecting");
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn prompt(text: &str) -> Result<String> {
    print!("{text}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}
