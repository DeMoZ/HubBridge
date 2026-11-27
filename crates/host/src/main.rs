//! `hubbridge` — desktop CLI for the HubBridge agent.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use hubbridge_host::serial::bench::{self, EchoOptions, RequestOptions};
use hubbridge_host::serial::bridge::{BridgeOptions, SerialBridge};
use hubbridge_host::serial::{Flow, Line, Parity, PortOptions, SerialConfig, SerialPort, StopBits, tcp};
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
    /// Install or update the HubBridge app on a phone connected by USB (adb), then open it.
    InstallApk {
        /// adb serial; needed only when several phones are connected.
        #[arg(long)]
        serial: Option<String>,
        /// APK to install (default: bundled HubBridge.apk, or the APK built in this checkout).
        #[arg(long)]
        apk: Option<std::path::PathBuf>,
        /// Do not open the app after installing.
        #[arg(long)]
        no_launch: bool,
        /// If the installed app has another signing key, uninstall it first (forgets its pairings).
        #[arg(long)]
        replace_incompatible: bool,
    },
    /// Talk to a USB device on the phone (open, control and bulk transfers).
    Usb {
        #[command(subcommand)]
        cmd: UsbCmd,
    },
    /// Use a USB serial device on the phone (CDC-ACM, CP210x, FTDI, CH34x) as a serial port
    /// on this computer: a PTY and/or a TCP port, or measure it with --bench-echo /
    /// --bench-request.
    Serial(SerialArgs),
    /// List paired agents.
    Agents,
    /// Forget a paired agent (also remove this computer in the phone app).
    Forget { agent: String },
}

#[derive(Subcommand)]
enum UsbCmd {
    /// Read the descriptors live from the device with control transfers.
    Descriptors {
        /// Device id from `hubbridge devices`.
        id: u32,
        target: Option<String>,
    },
    /// USB flash drive / card reader: identify it, show capacity, optionally measure read speed.
    /// The phone stops using the drive while HubBridge has it (it is unmounted there).
    Storage {
        id: u32,
        target: Option<String>,
        /// Read this many MiB from the start of the drive and report the speed.
        #[arg(long)]
        bench: Option<u64>,
        /// KiB per USB transfer in the data phase (all in flight together).
        #[arg(long, default_value_t = 64)]
        chunk_kib: u32,
        /// KiB per SCSI READ(10) command.
        #[arg(long, default_value_t = 1024)]
        command_kib: u32,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ParityArg {
    None,
    Odd,
    Even,
    Mark,
    Space,
}

#[derive(Clone, Copy, ValueEnum)]
enum FlowArg {
    None,
    Rtscts,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Args)]
struct SerialArgs {
    /// Device id from `hubbridge devices`.
    id: u32,
    target: Option<String>,
    #[arg(long, default_value_t = 115_200)]
    baud: u32,
    #[arg(long, default_value_t = 8)]
    data_bits: u8,
    #[arg(long, value_enum, default_value = "none")]
    parity: ParityArg,
    /// 1, 1.5 or 2.
    #[arg(long, default_value = "1")]
    stop_bits: String,
    #[arg(long, value_enum, default_value = "none")]
    flow: FlowArg,
    /// DTR when the port opens.
    #[arg(long, value_enum, default_value = "on")]
    dtr: OnOff,
    /// RTS when the port opens.
    #[arg(long, value_enum, default_value = "on")]
    rts: OnOff,
    /// Which port of a multi-port device (FTDI, dual CDC); 0 is the first.
    #[arg(long, default_value_t = 0)]
    port: usize,
    /// Serve a pseudo-terminal (macOS, Linux; the default there without --tcp).
    #[arg(long)]
    pty: bool,
    /// Serve TCP on this port (127.0.0.1) or address:port.
    #[arg(long, value_name = "ADDR|PORT")]
    tcp: Option<String>,
    /// With --tcp: speak RFC 2217 (baud rate, DTR/RTS, break) instead of a raw stream.
    #[arg(long)]
    rfc2217: bool,
    /// Ignore the line settings programs set on the PTY / RFC 2217 connection.
    #[arg(long)]
    lock_line: bool,
    /// IN transfers kept in flight.
    #[arg(long, default_value_t = 4)]
    in_flight: usize,
    /// FTDI latency timer (1-255 ms).
    #[arg(long, default_value_t = 16)]
    ftdi_latency_ms: u8,
    /// Measure round trips and throughput on a device that echoes (TX wired to RX).
    #[arg(long)]
    bench_echo: bool,
    /// Round trips for --bench-echo and rounds for --bench-request.
    #[arg(long)]
    count: Option<usize>,
    /// Bytes per round trip of --bench-echo.
    #[arg(long, default_value_t = 32)]
    size: usize,
    /// Bytes of the --bench-echo stream.
    #[arg(long, default_value_t = 1_048_576)]
    bytes: usize,
    /// Bytes unacknowledged at most during the --bench-echo stream.
    #[arg(long, default_value_t = 512)]
    window: usize,
    /// Send these bytes (hex) repeatedly and time the replies (needs --reply-len or --reply-idle-ms).
    #[arg(long, value_name = "HEX")]
    bench_request: Option<String>,
    /// A reply is complete after this many bytes.
    #[arg(long)]
    reply_len: Option<usize>,
    /// A reply is complete when the line is idle this long after the first byte.
    #[arg(long)]
    reply_idle_ms: Option<u64>,
    /// Milliseconds between requests.
    #[arg(long, default_value_t = 0)]
    interval_ms: u64,
    /// A round without a complete reply after this long counts as timed out.
    #[arg(long, default_value_t = 1000)]
    reply_timeout_ms: u64,
}

fn parse_stop_bits(s: &str) -> Result<StopBits> {
    Ok(match s {
        "1" => StopBits::One,
        "1.5" => StopBits::OnePointFive,
        "2" => StopBits::Two,
        other => bail!("--stop-bits is 1, 1.5 or 2, not '{other}'"),
    })
}

fn parse_hex(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace() && *c != ':').collect();
    hex::decode(clean.trim_start_matches("0x")).map_err(|e| anyhow::anyhow!("--bench-request is hex bytes: {e}"))
}

fn hex_string(b: &[u8]) -> String {
    hex::encode(b)
}

fn ms_f(d: Duration) -> String {
    format::ms(d)
}

async fn serial(a: SerialArgs, store: &mut Store) -> Result<()> {
    let measuring = a.bench_echo || a.bench_request.is_some();
    let tcp_addr = a.tcp.as_deref().map(tcp::parse_listen).transpose()?;
    if a.rfc2217 && tcp_addr.is_none() {
        bail!("--rfc2217 needs --tcp");
    }
    // PTY by default where there is one and nothing else was asked for.
    let pty = a.pty || (cfg!(unix) && tcp_addr.is_none() && !measuring);
    if !measuring && pty && !cfg!(unix) {
        bail!("a pseudo-terminal is not available on this platform: use --tcp PORT (with --rfc2217 for line control)");
    }
    if !measuring && !pty && tcp_addr.is_none() {
        bail!("nothing to serve: use --pty (macOS, Linux) or --tcp PORT");
    }
    let config = SerialConfig {
        line: Line {
            baud: a.baud,
            data_bits: a.data_bits,
            parity: match a.parity {
                ParityArg::None => Parity::None,
                ParityArg::Odd => Parity::Odd,
                ParityArg::Even => Parity::Even,
                ParityArg::Mark => Parity::Mark,
                ParityArg::Space => Parity::Space,
            },
            stop_bits: parse_stop_bits(&a.stop_bits)?,
        },
        flow: match a.flow {
            FlowArg::None => Flow::None,
            FlowArg::Rtscts => Flow::RtsCts,
        },
        dtr: matches!(a.dtr, OnOff::On),
        rts: matches!(a.rts, OnOff::On),
    };
    let port_opts = PortOptions {
        port: a.port,
        in_flight: a.in_flight.clamp(1, 32),
        ftdi_latency_ms: a.ftdi_latency_ms.max(1),
        ..PortOptions::default()
    };
    let request = a.bench_request.as_deref().map(parse_hex).transpose()?;

    let client = Arc::new(connect(a.target.as_deref(), store).await?);
    let info = device_info(&client, a.id).await?;

    if measuring {
        let port = SerialPort::open(Arc::clone(&client), &info, config, port_opts)
            .await
            .map_err(permission_hint)?;
        let pi = port.info();
        println!(
            "{} on {} at {} over {}",
            pi.label,
            info.label(),
            pi.line,
            if client.addr.ip().is_loopback() {
                "ADB / localhost"
            } else {
                "Wi-Fi/LAN"
            }
        );
        let res = measure(&port, &a, request).await;
        port.drain().await;
        let _ = port.refresh_errors().await;
        let s = port.stats();
        println!(
            "counters: {} bytes sent, {} received, {} dropped, {} line errors",
            s.tx,
            s.rx,
            s.rx_dropped,
            s.line_errors()
        );
        port.close().await;
        res?;
        return Ok(());
    }

    let bridge = SerialBridge::start(
        Arc::clone(&client),
        &info,
        BridgeOptions {
            config,
            port: port_opts,
            pty,
            tcp: tcp_addr,
            rfc2217: a.rfc2217,
            lock_line: a.lock_line,
        },
    )
    .await
    .map_err(permission_hint)?;
    let bi = bridge.info();
    println!("{} on {}", bi.label, info.label());
    println!(
        "line     {}, flow {}",
        bi.line,
        if bi.flow == Flow::RtsCts { "RTS/CTS" } else { "none" }
    );
    if let Some(p) = &bi.pty_path {
        println!("PTY      {p}");
    }
    if let Some(t) = bi.tcp_addr {
        println!("TCP      {t}{}", if bi.rfc2217 { " (RFC 2217)" } else { "" });
    }
    if let Some(w) = &bi.warning {
        eprintln!("warning: {w}");
    }
    println!("Ctrl-C to stop.");

    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut seq = 0u32;
    let outcome: Result<()> = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            reason = bridge.done() => break Err(anyhow::anyhow!("the serial bridge ended: {reason}")),
            _ = tick.tick() => {
                // The agent closes a session that sends nothing for 20 s.
                seq += 1;
                if let Err(e) = client.ping(seq).await {
                    break Err(anyhow::anyhow!("the connection to the phone is gone: {e:#}"));
                }
            }
        }
    };
    bridge.stop().await;
    drop(bridge);
    if let Ok(c) = Arc::try_unwrap(client) {
        c.close().await;
    }
    outcome
}

async fn measure(port: &SerialPort, a: &SerialArgs, request: Option<Vec<u8>>) -> Result<()> {
    if a.bench_echo {
        let mut o = EchoOptions {
            size: a.size,
            bytes: a.bytes,
            window: a.window,
            ..EchoOptions::default()
        };
        if let Some(c) = a.count {
            o.count = c;
        }
        let r = bench::bench_echo(port, &o).await?;
        match r.latency {
            Some(l) => println!(
                "latency  {} round trips of {} bytes: p50 {} / p95 {} / max {}",
                r.rounds,
                o.size,
                ms_f(l.p50),
                ms_f(l.p95),
                ms_f(l.max)
            ),
            None => println!("latency  no round trip completed"),
        }
        println!(
            "stream   {} bytes in {:.2} s: {:.1} KB/s ({:.0}% of the line rate of {})",
            r.stream_bytes,
            r.elapsed.as_secs_f64(),
            r.kb_per_s,
            r.line_rate_percent,
            port.line()
        );
        println!("{} bytes lost, {} wrong", r.lost, r.wrong);
    }
    if let Some(request) = request {
        let o = RequestOptions {
            request,
            reply_len: a.reply_len,
            reply_idle: a.reply_idle_ms.map(Duration::from_millis),
            count: a.count.unwrap_or(100),
            interval: Duration::from_millis(a.interval_ms),
            timeout: Duration::from_millis(a.reply_timeout_ms),
        };
        let r = bench::bench_request(port, &o).await?;
        match &r.first_reply {
            Some(b) => println!("reply    {} ({} bytes)", hex_string(b), b.len()),
            None => println!("reply    none"),
        }
        match r.latency {
            Some(l) => println!(
                "latency  {} of {} rounds: p50 {} / p95 {} / max {}",
                r.rounds - r.timeouts,
                r.rounds,
                ms_f(l.p50),
                ms_f(l.p95),
                ms_f(l.max)
            ),
            None => println!("latency  no round got a reply"),
        }
        println!(
            "{} rounds timed out; reply throughput {:.0} bytes/s",
            r.timeouts, r.reply_bytes_per_s
        );
    }
    Ok(())
}

async fn device_info(c: &Client, id: u32) -> Result<hubbridge_proto::UsbDeviceInfo> {
    match c.list_devices().await?.into_iter().find(|d| d.id == id) {
        Some(d) => Ok(d),
        None => bail!("no device {id} on {} (see `hubbridge devices`)", c.agent_name),
    }
}

fn permission_hint(e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<hubbridge_host::client::AgentError>() {
        Some(a) if a.code == hubbridge_proto::ErrorCode::NoPermission => {
            e.context("no USB permission on the phone: run `hubbridge grant <id>` and tap Allow")
        }
        _ => e,
    }
}

async fn usb(cmd: UsbCmd, store: &mut Store) -> Result<()> {
    match cmd {
        UsbCmd::Descriptors { id, target } => {
            let c = connect(target.as_deref(), store).await?;
            let info = device_info(&c, id).await?;
            c.open_device(id).await.map_err(permission_hint)?;
            let res = hubbridge_host::msc::read_descriptors(&c, id).await;
            let _ = c.close_device(id).await;
            let (raw, strings) = res?;
            let mut live = info.clone();
            live.raw_descriptors = Some(raw.clone());
            live.apply_raw_descriptors()
                .map_err(|e| anyhow::anyhow!("descriptors from the device: {e}"))?;
            print!("{}", format::device(&live));
            for (i, s) in strings {
                println!("    string {i}: {s:?}");
            }
            let same = info.raw_descriptors.as_deref() == Some(raw.as_slice());
            println!(
                "{} bytes read live; {}",
                raw.len(),
                if same {
                    "identical to what Android reported"
                } else {
                    "Android's cached copy differs or is missing"
                }
            );
            c.close().await;
        }
        UsbCmd::Storage {
            id,
            target,
            bench,
            chunk_kib,
            command_kib,
        } => {
            let c = connect(target.as_deref(), store).await?;
            let info = device_info(&c, id).await?;
            let mut ms = hubbridge_host::msc::MassStorage::open(&c, &info)
                .await
                .map_err(permission_hint)?;
            ms.chunk = chunk_kib.clamp(1, 1024) * 1024;
            let res = async {
                let inq = ms.inquiry().await?;
                println!(
                    "{} {} rev {}{}",
                    inq.vendor,
                    inq.product,
                    inq.revision,
                    if inq.removable { " (removable)" } else { "" }
                );
                let cap = ms.read_capacity().await?;
                println!(
                    "capacity {:.2} GB ({} blocks of {} bytes)",
                    cap.bytes() as f64 / 1e9,
                    cap.blocks,
                    cap.block_size
                );
                if let Some(mib) = bench {
                    let r = ms
                        .bench(cap, mib * 1024 * 1024, Some(command_kib.max(1) * 1024))
                        .await?;
                    println!(
                        "read {:.1} MiB in {:.2} s: {:.1} MB/s over {}",
                        r.bytes as f64 / 1048576.0,
                        r.elapsed.as_secs_f64(),
                        r.mb_per_s(),
                        if c.addr.ip().is_loopback() {
                            "ADB / localhost"
                        } else {
                            "Wi-Fi/LAN"
                        }
                    );
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let _ = ms.close().await;
            c.close().await;
            res?;
        }
    }
    Ok(())
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
        Cmd::InstallApk {
            serial,
            apk,
            no_launch,
            replace_incompatible,
        } => {
            let opts = hubbridge_host::install::InstallOptions {
                serial,
                apk,
                launch: !no_launch,
                replace_incompatible,
            };
            let target = hubbridge_host::adb::pick(opts.serial.as_deref()).await?;
            let src = hubbridge_host::install::find_apk(opts.apk.as_deref())?;
            println!(
                "Installing {} ({}) on {} ...",
                src.path.display(),
                src.origin,
                target.label()
            );
            let r = hubbridge_host::install::install(&opts).await?;
            let was = r
                .previous_version
                .as_deref()
                .map(|v| format!(" (was {v})"))
                .unwrap_or_default();
            println!("Installed HubBridge {}{was}.", r.version.as_deref().unwrap_or("?"));
            if r.reinstalled {
                println!("The old app was removed first: pair this computer again.");
            }
            if r.launched {
                println!("Opened on the phone: tap Start, then Pair a new computer.");
            }
        }
        Cmd::Usb { cmd } => usb(cmd, &mut store).await?,
        Cmd::Serial(a) => serial(a, &mut store).await?,
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
