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
//!
//! Fake devices: 1002 CP210x (CP2102, loops TX back to RX), 1003 HID keyboard without
//! permission, 1004 flash drive, 1005 FT232R, 1006 FT2232H (two ports), 1007 FT232H,
//! 1008 CH340, 1009 CDC-ACM. The UART adapters and the CDC device answer their chip's control
//! requests and loop bulk data back, so `hubbridge serial <id>` works against each.
//!
//! Displays and mirroring: `--display "HDMI Screen:1280x720@60"` (repeatable, ids from 8)
//! adds fake monitors that accept mirror streams; the video is checked, not shown.
//! `--video-out <file.h264>` keeps the received stream, `--decode-delay-ms` makes the decoder
//! slow, `--display-churn <s>` unplugs and replugs the first display, `--lock-after <s>`
//! reports the phone as locked.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use hubbridge_agent::fake::{FakeKind, FakeUsb, FakeVideoSink, SerialChip};
use hubbridge_agent::{Agent, AgentCallbacks, AgentConfig};
use hubbridge_proto::{
    DEFAULT_PORT, DisplayInfo, DisplayList, DisplayMode, MDNS_SERVICE_TYPE, PROTO_VERSION, fixtures,
};
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
    /// A fake display, "<name>:<width>x<height>@<hz>"; repeatable, ids from 8.
    #[arg(long = "display", value_parser = parse_display)]
    displays: Vec<FakeDisplay>,
    /// Every N seconds unplug/replug the first fake display.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    display_churn: Option<u64>,
    /// Time the fake video decoder needs per frame.
    #[arg(long, default_value_t = 0)]
    decode_delay_ms: u64,
    /// Write the received video stream (H.264 Annex B) to this file.
    #[arg(long)]
    video_out: Option<PathBuf>,
    /// Report the phone as locked after N seconds.
    #[arg(long)]
    lock_after: Option<u64>,
}

/// Android gives external displays ids like these.
const FIRST_DISPLAY_ID: u32 = 8;

#[derive(Debug, Clone, PartialEq)]
struct FakeDisplay {
    name: String,
    width: u32,
    height: u32,
    hz: f64,
}

fn parse_display(spec: &str) -> Result<FakeDisplay, String> {
    let wrong = || format!("`{spec}` is not <name>:<width>x<height>@<hz>");
    let (name, mode) = spec.rsplit_once(':').ok_or_else(wrong)?;
    let (size, hz) = mode.split_once('@').ok_or_else(wrong)?;
    let (width, height) = size.split_once('x').ok_or_else(wrong)?;
    let width: u32 = width.parse().map_err(|_| wrong())?;
    let height: u32 = height.parse().map_err(|_| wrong())?;
    let hz: f64 = hz.parse().map_err(|_| wrong())?;
    if name.is_empty() || width == 0 || height == 0 || !(hz > 0.0 && hz <= 1000.0) {
        return Err(wrong());
    }
    Ok(FakeDisplay {
        name: name.to_string(),
        width,
        height,
        hz,
    })
}

/// The list the agent reports; `first_present` is false while the first display is unplugged.
fn display_list(displays: &[FakeDisplay], first_present: bool) -> DisplayList {
    let displays = displays
        .iter()
        .zip(FIRST_DISPLAY_ID..)
        .skip(usize::from(!first_present))
        .map(|(d, id)| {
            let refresh_mhz = (d.hz * 1000.0).round() as u32;
            DisplayInfo {
                id,
                name: d.name.clone(),
                width: d.width,
                height: d.height,
                refresh_mhz,
                modes: vec![DisplayMode {
                    width: d.width,
                    height: d.height,
                    refresh_mhz,
                }],
                presentation: true,
                secure: false,
            }
        })
        .collect();
    DisplayList {
        displays,
        desktop_mode: false,
    }
}

/// One line per second for every stream that took frames or is still running.
async fn print_video(sink: Arc<FakeVideoSink>) {
    let mut before = std::collections::BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        for (stream, log) in sink.logs() {
            let last = before.insert(stream, log.clone()).unwrap_or_default();
            if log.stops > 0 {
                if last.stops == 0 {
                    info!(
                        display = log.display_id,
                        decoded = log.decoded,
                        bad = log.bad,
                        "mirror stopped"
                    );
                }
                continue;
            }
            info!(
                display = log.display_id,
                fps = log.decoded - last.decoded,
                decoded = log.decoded,
                keyframes = log.keyframes,
                bad = log.bad,
                "mirror"
            );
        }
    }
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
    let drive = fixtures::flash_drive(1004);
    let extra = [
        fixtures::ft232r_uart(1005),
        fixtures::ft2232h_uart(1006),
        fixtures::ft232h_uart(1007),
        fixtures::ch340_uart(1008),
        fixtures::cdc_acm_simple(1009),
    ];
    let mut all = vec![uart.clone(), keyboard.clone(), drive.clone()];
    all.extend(extra.iter().cloned());
    agent.set_devices(all);

    // Fake USB behind them: the UARTs answer their chip's requests and loop bulk data back,
    // the drive answers SCSI (32 MiB).
    let usb = FakeUsb::new();
    usb.add(
        1002,
        fixtures::cp2102_raw(),
        &["Silicon Labs", "CP2102 USB to UART Bridge Controller", "0001"],
        FakeKind::Serial(SerialChip::Cp210x { partnum: 2 }),
        true,
    );
    for (id, raw, strings, chip) in [
        (
            1005,
            fixtures::ft232r_raw(),
            ["FTDI", "FT232R USB UART", "A50285BI"],
            SerialChip::Ftdi,
        ),
        (
            1006,
            fixtures::ft2232h_raw(),
            ["FTDI", "Dual RS232-HS", "FT2232H01"],
            SerialChip::Ftdi,
        ),
        (
            1007,
            fixtures::ft232h_raw(),
            ["FTDI", "Single RS232-HS", "FT232H001"],
            SerialChip::Ftdi,
        ),
        (
            1008,
            fixtures::ch340_raw(),
            ["wch.cn", "USB Serial", "0001"],
            SerialChip::Ch34x {
                version: 0x31,
                limited_prescaler: false,
            },
        ),
        (
            1009,
            fixtures::cdc_acm_simple_raw(),
            ["HubBridge", "CDC ACM Serial", "CDC0001"],
            SerialChip::CdcAcm,
        ),
    ] {
        usb.add(id, raw, &strings, FakeKind::Serial(chip), true);
    }
    usb.add(
        1003,
        fixtures::hid_keyboard_raw(),
        &["Logitech", "USB Keyboard", "KBD-42"],
        FakeKind::DescriptorsOnly,
        false,
    );
    usb.add(
        1004,
        fixtures::flash_drive_raw(),
        &["Generic", "Mass Storage Device", "121220160204"],
        FakeKind::MassStorage { blocks: 65536 },
        true,
    );
    agent.set_usb_backend(usb);

    agent.set_displays(display_list(&args.displays, true));
    let video = Arc::new(FakeVideoSink::default());
    video.set_delay(Duration::from_millis(args.decode_delay_ms));
    video.set_output(args.video_out.clone());
    agent.set_video_sink(video.clone());
    tokio::spawn(print_video(video));
    if let Some(secs) = args.display_churn {
        let agent = agent.clone();
        let displays = args.displays.clone();
        tokio::spawn(async move {
            let mut present = true;
            loop {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                present = !present;
                info!(first_display = present, "churn: display list changed");
                agent.set_displays(display_list(&displays, present));
            }
        });
    }
    if let Some(secs) = args.lock_after {
        let agent = agent.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            info!("the phone is now \"locked\"");
            agent.set_phone_locked(true);
        });
    }

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
                let mut list = vec![uart.clone(), drive.clone()];
                list.extend(extra.iter().cloned());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_argument() {
        let d = parse_display("HDMI Screen:1280x720@60").unwrap();
        assert_eq!(
            d,
            FakeDisplay {
                name: "HDMI Screen".into(),
                width: 1280,
                height: 720,
                hz: 60.0
            }
        );
        assert_eq!(parse_display("a:b:640x480@59.94").unwrap().name, "a:b");
        for bad in [
            "HDMI Screen:1280x720",
            "HDMI Screen",
            ":1280x720@60",
            "x:0x720@60",
            "x:1280x0@60",
            "x:1280x720@0",
            "x:1280@60",
            "x:1280x720@NaN",
        ] {
            assert!(parse_display(bad).is_err(), "{bad}");
        }
        let list = display_list(&[d.clone(), d], false);
        assert_eq!(list.displays.len(), 1);
        assert_eq!((list.displays[0].id, list.displays[0].refresh_mhz), (9, 60_000));
    }
}
