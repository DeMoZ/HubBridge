//! What `hubbridge displays` and `hubbridge mirror` do, with the output as a parameter so
//! that tests run the same code.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hubbridge_proto::{DisplayInfo, DisplayList};
use tokio::sync::mpsc;

use super::fit::preset_bitrate_kbps;
use super::pace::Gate;
use super::session::{self, MirrorHandle, MirrorOptions, MirrorSession};
use super::source::{EncodeConfig, FrameSource, SourceEvent};
use super::stats::Stats;
use crate::client::{Client, ClientEvent};
use crate::store::Store;

/// A control session without an answer for this long is gone.
const CONTROL_SILENCE: Duration = Duration::from_secs(5);

/// `60 Hz`, `59.94 Hz`.
pub fn hz(refresh_mhz: u32) -> String {
    let text = format!("{:.2}", f64::from(refresh_mhz) / 1000.0);
    format!("{} Hz", text.trim_end_matches('0').trim_end_matches('.'))
}

fn display_line(d: &DisplayInfo, desktop_mode: bool) -> String {
    let flags: Vec<&str> = [(d.presentation, "presentation"), (d.secure, "secure")]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect();
    format!(
        "[{}] {}  {}×{}  {}  {}  DeX {}",
        d.id,
        d.name,
        d.width,
        d.height,
        hz(d.refresh_mhz),
        if flags.is_empty() { "-".into() } else { flags.join(", ") },
        if desktop_mode { "on" } else { "off" },
    )
}

/// The listing of `hubbridge displays` (also used by `hubbridge watch`).
pub fn print_displays(list: &DisplayList, phone: &str, json: bool, out: &mut dyn Write) -> Result<()> {
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(list)?)?;
        return Ok(());
    }
    if list.displays.is_empty() {
        writeln!(out, "There are no displays on {phone}: attach a monitor to the phone.")?;
    }
    for d in &list.displays {
        writeln!(out, "{}", display_line(d, list.desktop_mode))?;
    }
    if list.desktop_mode {
        writeln!(
            out,
            "Samsung DeX is on: the monitor shows the DeX desktop. Turn DeX off to mirror."
        )?;
    }
    Ok(())
}

/// `hubbridge displays`.
pub async fn displays(client: &Client, json: bool, out: &mut dyn Write) -> Result<()> {
    // An older app would drop the session on the question.
    session::require_new_app(&client.agent_info().await?.app_version)?;
    print_displays(&client.displays().await?, &client.agent_name, json, out)
}

/// Keeps the control session alive and notices when it is gone; passes its events to the
/// mirror.
async fn watch_control(client: Arc<Client>, handle: Arc<MirrorHandle>) {
    let mut events = client.take_events();
    let pinger = {
        let handle = Arc::clone(&handle);
        let client = Arc::clone(&client);
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut last_ok = Instant::now();
            for seq in 1.. {
                tick.tick().await;
                match tokio::time::timeout(Duration::from_secs(1), client.ping(seq)).await {
                    Ok(Ok(_)) => last_ok = Instant::now(),
                    _ if last_ok.elapsed() >= CONTROL_SILENCE => {
                        handle.on_control_event(&ClientEvent::Disconnected("no answer".into()));
                        break;
                    }
                    _ => {}
                }
            }
            std::future::pending::<()>().await;
        }
    };
    let forward = async {
        if let Some(events) = &mut events {
            while let Some(event) = events.recv().await {
                handle.on_control_event(&event);
            }
        }
        std::future::pending::<()>().await;
    };
    tokio::join!(pinger, forward);
}

/// `hubbridge mirror` onto a phone: runs until `stop` completes, `seconds` pass or the mirror
/// ends by itself, printing the readout every second. An error when it ended by itself.
#[allow(clippy::too_many_arguments)]
pub async fn mirror(
    client: Arc<Client>,
    store: &mut Store,
    display_id: u32,
    options: MirrorOptions,
    source: Box<dyn FrameSource>,
    seconds: Option<u64>,
    stop: impl Future<Output = ()>,
    out: &mut dyn Write,
) -> Result<()> {
    let handle = Arc::new(MirrorSession::start(&client, store, display_id, options, source).await?);
    let config = handle.config();
    writeln!(
        out,
        "Mirroring to {} [{}] as {}×{} at {} fps, {} kbit/s. Ctrl-C stops.",
        handle.display().name,
        display_id,
        config.width,
        config.height,
        config.fps,
        config.bitrate_kbps
    )?;
    let watcher = tokio::spawn(watch_control(Arc::clone(&client), Arc::clone(&handle)));
    let limit = async {
        match seconds {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(limit, stop);
    let mut stats = handle.stats();
    let mut stats_open = true;
    let ending = loop {
        tokio::select! {
            _ = &mut stop => break handle.stop().await,
            _ = &mut limit => break handle.stop().await,
            ending = handle.done() => break ending,
            changed = stats.changed(), if stats_open => match changed {
                Ok(()) => {
                    let snapshot = stats.borrow_and_update().clone();
                    if let Some(s) = snapshot {
                        writeln!(out, "{s}")?;
                    }
                }
                Err(_) => stats_open = false,
            },
        }
    };
    watcher.abort();
    let _ = watcher.await;
    drop(handle);
    if let Ok(client) = Arc::try_unwrap(client) {
        client.close().await;
    }
    if ending.by_user {
        writeln!(out, "Mirroring stopped.")?;
        Ok(())
    } else {
        Err(anyhow!("Mirroring stopped: {}", ending.reason))
    }
}

/// `hubbridge mirror --to-file`: encode into a file on this computer, no phone involved.
/// The picture is fitted to `size`.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    mut source: Box<dyn FrameSource>,
    path: &Path,
    size: (u32, u32),
    options: MirrorOptions,
    seconds: Option<u64>,
    stop: impl Future<Output = ()>,
    out: &mut dyn Write,
) -> Result<()> {
    let native = source.native_size()?;
    let (width, height) = source.stream_size(size.0, size.1)?;
    if width == 0 || height == 0 {
        bail!(
            "cannot fit a {}x{} picture into {}x{}",
            native.0,
            native.1,
            size.0,
            size.1
        );
    }
    let bitrate_kbps = options
        .bitrate_kbps
        .unwrap_or_else(|| preset_bitrate_kbps(width, height, options.fps));
    let mut file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let gate = Gate::new();
    let (tx, mut frames) = mpsc::unbounded_channel();
    let config = EncodeConfig {
        width,
        height,
        fps: options.fps,
        bitrate_kbps,
    };
    source.start(
        config,
        gate.clone(),
        Arc::new(move |event| {
            let _ = tx.send(event);
        }),
    )?;
    writeln!(
        out,
        "Recording {}x{} ({}x{} source) at {} fps, {bitrate_kbps} kbit/s into {}. Ctrl-C stops.",
        width,
        height,
        native.0,
        native.1,
        options.fps,
        path.display()
    )?;
    let limit = async {
        match seconds {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(limit, stop);
    let mut readout = Stats::new(width, height, Instant::now());
    let mut second = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let mut kick = tokio::time::interval(Duration::from_millis(20));
    let mut pictures = 0u64;
    let result: Result<()> = loop {
        tokio::select! {
            _ = &mut stop => break Ok(()),
            _ = &mut limit => break Ok(()),
            Some(event) = frames.recv() => match event {
                SourceEvent::Failed(why) => break Err(anyhow!("the screen capture stopped: {why}")),
                SourceEvent::Frame(frame) => {
                    if let Err(e) = file.write_all(&frame.data) {
                        break Err(e).context("write the file");
                    }
                    // The file takes a frame at once.
                    if !frame.config {
                        let seq = gate.output();
                        readout.on_sent(seq, frame.captured, frame.data.len(), frame.keyframe);
                        gate.ack(seq);
                        pictures += 1;
                        source.kick();
                    }
                }
            },
            _ = kick.tick() => source.kick(),
            _ = second.tick() => {
                let s = readout.take(Instant::now(), gate.skipped());
                writeln!(out, "{:.0} fps  {:.0} kbit/s  skipped {}  keyframes {}", s.fps, s.kbps, s.skipped, s.keyframes)?;
            }
        }
    };
    tokio::task::spawn_blocking(move || source.stop()).await?;
    file.flush()?;
    result?;
    writeln!(out, "Recorded {pictures} pictures.")?;
    if pictures == 0 {
        bail!("no picture arrived");
    }
    Ok(())
}
