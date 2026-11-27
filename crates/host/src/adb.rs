//! ADB: wired mode (`adb forward tcp:<local> tcp:<agent port>`), ADB over Wi-Fi, installs.
//!
//! A phone with one USB-C port that hosts the hub cannot be cabled to the computer at the
//! same time (docs/LIMITATIONS.md), so ADB over Wi-Fi matters here: enable it once over the
//! cable (`adb tcpip 5555`, then `adb connect ip:5555`) or pair with Android 11+ wireless
//! debugging (`adb pair ip:port code`); after that installs and `adb forward` work while the
//! phone sits on the hub. The approach follows UniversalKM's proven adb handling.
//!
//! We use the user's own adb and its shared server (127.0.0.1:5037) and never kill it.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::process::Command;

/// Port adbd listens on after `adb tcpip`.
pub const TCPIP_PORT: u16 = 5555;
/// Default time limit of one adb command.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

fn adb_exe() -> &'static str {
    if cfg!(windows) { "adb.exe" } else { "adb" }
}

/// Where adb usually lives. An app started from Finder / the Start menu has no shell PATH,
/// so the common install locations are checked explicitly.
fn common_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        if let Some(h) = &home {
            dirs.push(h.join("Library/Android/sdk/platform-tools"));
        }
    } else if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Android\\Sdk\\platform-tools"));
        }
        dirs.push(PathBuf::from("C:\\platform-tools"));
    } else if let Some(h) = &home {
        dirs.push(h.join("Android/Sdk/platform-tools"));
        dirs.push(PathBuf::from("/usr/bin"));
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(sdk) = std::env::var_os(var) {
            dirs.push(PathBuf::from(sdk).join("platform-tools"));
        }
    }
    dirs
}

/// `$ADB`, then `adb` on PATH, then the common locations. `None` if adb is not installed.
pub fn find_adb() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ADB") {
        return Some(PathBuf::from(p));
    }
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    path_dirs
        .into_iter()
        .chain(common_dirs())
        .map(|d| d.join(adb_exe()))
        .find(|p| p.is_file())
}

/// adb to run (falls back to the bare name, so the error says what is missing).
pub fn adb_path() -> PathBuf {
    find_adb().unwrap_or_else(|| PathBuf::from(adb_exe()))
}

/// How to install adb, for messages.
pub fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "install it with `brew install android-platform-tools`"
    } else if cfg!(windows) {
        "install Android SDK Platform-Tools and add them to PATH (or set ADB)"
    } else {
        "install the android-tools / adb package"
    }
}

/// Run adb with a time limit; on failure the error carries adb's stderr (or stdout).
pub async fn adb_with_timeout(serial: Option<&str>, args: &[&str], limit: Duration) -> Result<String> {
    let Some(exe) = find_adb() else {
        bail!("adb not found: {}", install_hint())
    };
    let mut cmd = Command::new(exe);
    if let Some(s) = serial {
        cmd.args(["-s", s]);
    }
    cmd.args(args).kill_on_drop(true);
    #[cfg(windows)]
    {
        // No console window flashing up from the GUI.
        cmd.creation_flags(0x0800_0000);
    }
    let out = tokio::time::timeout(limit, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("adb {}: timed out after {} s", args.join(" "), limit.as_secs()))?
        .context("run adb")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("adb {}: {msg}", args.join(" "));
    }
    Ok(stdout)
}

/// Run adb with [`DEFAULT_TIMEOUT`].
pub async fn adb(serial: Option<&str>, args: &[&str]) -> Result<String> {
    adb_with_timeout(serial, args, DEFAULT_TIMEOUT).await
}

/// One line of `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdbDevice {
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, ...
    pub state: String,
    /// `model:` field with `_` turned back into spaces, e.g. "SM G970F".
    pub model: Option<String>,
    /// Reached over the network (`ip:port` or a wireless-debugging mDNS name).
    pub network: bool,
}

impl AdbDevice {
    pub fn label(&self) -> String {
        match &self.model {
            Some(m) => format!("{m} ({})", self.serial),
            None => self.serial.clone(),
        }
    }
}

/// `ip:port` serials (adb connect) and `adb-XXXX._adb-tls-connect._tcp` (wireless debugging).
pub fn is_network_serial(serial: &str) -> bool {
    serial.contains(':') || serial.contains("._adb-tls-connect.")
}

/// Parse `adb devices -l` output.
pub fn parse_devices(out: &str) -> Vec<AdbDevice> {
    out.lines()
        .filter(|l| !l.starts_with("List of devices") && !l.starts_with('*'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?.to_string();
            let state = it.next()?.to_string();
            let model = it.find_map(|f| f.strip_prefix("model:")).map(|m| m.replace('_', " "));
            let network = is_network_serial(&serial);
            Some(AdbDevice {
                serial,
                state,
                model,
                network,
            })
        })
        .collect()
}

/// All devices adb sees, in any state.
pub async fn list() -> Result<Vec<AdbDevice>> {
    Ok(parse_devices(&adb(None, &["devices", "-l"]).await?))
}

/// Serials of devices in the `device` state.
pub async fn devices() -> Result<Vec<String>> {
    Ok(list()
        .await?
        .into_iter()
        .filter(|d| d.state == "device")
        .map(|d| d.serial)
        .collect())
}

/// The device to work with: the given serial, or the only ready one.
pub async fn pick(serial: Option<&str>) -> Result<AdbDevice> {
    let all = list().await?;
    if let Some(s) = serial {
        return match all.into_iter().find(|d| d.serial == s) {
            Some(d) if d.state == "device" => Ok(d),
            Some(d) => bail!("{} is {}: {}", d.label(), d.state, state_hint(&d.state)),
            None => bail!("no adb device with serial {s}"),
        };
    }
    let ready: Vec<_> = all.iter().filter(|d| d.state == "device").cloned().collect();
    match ready.len() {
        1 => Ok(ready[0].clone()),
        0 => match all.first() {
            Some(d) => bail!("{} is {}: {}", d.label(), d.state, state_hint(&d.state)),
            None => bail!("no phone in `adb devices`: connect it by USB and turn on USB debugging"),
        },
        _ => {
            let names: Vec<_> = ready.iter().map(|d| d.label()).collect();
            bail!("several phones connected ({}): pick one by serial", names.join(", "))
        }
    }
}

pub fn state_hint(state: &str) -> &'static str {
    match state {
        "unauthorized" => "unlock the phone and allow USB debugging for this computer",
        "offline" => "reconnect the cable or restart adb (`adb kill-server`)",
        "no" | "no permissions" => "adb has no permission to open the device",
        "authorizing" | "connecting" => "wait a moment",
        _ => "check the phone",
    }
}

/// Forward a local port to the agent port on the phone; reuses an existing forward.
/// Returns the local address to connect to.
pub async fn forward(serial: Option<&str>, agent_port: u16) -> Result<SocketAddr> {
    let serial = pick(serial).await?.serial;
    let remote = format!("tcp:{agent_port}");
    let list = adb(None, &["forward", "--list"]).await?;
    for line in list.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let [s, local, r] = parts[..]
            && s == serial
            && r == remote
            && let Some(port) = local.strip_prefix("tcp:").and_then(|p| p.parse::<u16>().ok())
        {
            return Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        }
    }
    let out = adb(Some(&serial), &["forward", "tcp:0", &remote]).await?;
    let port: u16 = out
        .trim()
        .parse()
        .with_context(|| format!("unexpected adb forward output: {out:?}"))?;
    Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

// ---------------------------------------------------------------------------------------
// Identity and ADB over Wi-Fi
// ---------------------------------------------------------------------------------------

/// `ro.serialno`: the same phone seen over USB and over Wi-Fi has one value.
pub async fn phone_serial(serial: &str) -> Option<String> {
    let out = adb_with_timeout(
        Some(serial),
        &["shell", "getprop", "ro.serialno"],
        Duration::from_secs(5),
    )
    .await
    .ok()?;
    let s = out.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// First IPv4 of `wlan0` in `ip -4 addr show` output.
pub fn parse_wlan_ip(out: &str) -> Option<Ipv4Addr> {
    out.split_whitespace()
        .skip_while(|w| *w != "inet")
        .nth(1)?
        .split('/')
        .next()?
        .parse()
        .ok()
}

/// The phone's Wi-Fi IPv4 address.
pub async fn wifi_ip(serial: &str) -> Result<Ipv4Addr> {
    let out = adb_with_timeout(
        Some(serial),
        &["shell", "ip", "-4", "addr", "show", "wlan0"],
        Duration::from_secs(5),
    )
    .await?;
    parse_wlan_ip(&out).context("the phone has no Wi-Fi address: is Wi-Fi on?")
}

/// `adb connect addr` (adb prints errors on stdout with exit code 0, so check the text).
pub async fn connect(addr: &str) -> Result<()> {
    let out = adb_with_timeout(None, &["connect", addr], Duration::from_secs(8)).await?;
    if out.contains("connected to") {
        Ok(())
    } else if out.contains("refused") {
        bail!("{addr} refused: ADB over Wi-Fi is off (the phone restarted?); plug the cable once and enable it again")
    } else {
        bail!("adb connect {addr}: {}", out.trim())
    }
}

pub async fn disconnect(addr: &str) -> Result<()> {
    adb_with_timeout(None, &["disconnect", addr], Duration::from_secs(5))
        .await
        .map(|_| ())
}

/// Over the cable: switch adbd to TCP and connect to it over Wi-Fi. Returns `ip:5555`.
pub async fn enable_wifi(usb_serial: &str) -> Result<String> {
    let ip = wifi_ip(usb_serial).await?;
    adb(Some(usb_serial), &["tcpip", &TCPIP_PORT.to_string()]).await?;
    // adbd restarts in TCP mode; every transport of this phone drops for a moment.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let addr = format!("{ip}:{TCPIP_PORT}");
    let mut last = None;
    for _ in 0..3 {
        match connect(&addr).await {
            Ok(()) => return Ok(addr),
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("adb connect {addr} failed")))
}

/// Android 11+ wireless debugging: "Pair device with pairing code" shows `ip:port` and a code.
pub async fn pair(addr: &str, code: &str) -> Result<()> {
    let out = adb_with_timeout(None, &["pair", addr, code], Duration::from_secs(20)).await?;
    if out.contains("Successfully paired") {
        Ok(())
    } else {
        bail!("adb pair {addr}: {}", out.trim())
    }
}

/// `adb mdns services`: wireless-debugging phones announce `_adb-tls-connect._tcp ip:port`.
pub fn parse_mdns_connect(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let parts: Vec<&str> = l.split_whitespace().collect();
            match parts[..] {
                [name, kind, addr, ..] if kind.starts_with("_adb-tls-connect.") => {
                    Some((name.to_string(), addr.to_string()))
                }
                _ => None,
            }
        })
        .collect()
}

pub async fn mdns_connect_services() -> Vec<(String, String)> {
    adb_with_timeout(None, &["mdns", "services"], Duration::from_secs(5))
        .await
        .map(|o| parse_mdns_connect(&o))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_l() {
        let out = "List of devices attached\n\
            R28M303RHET            device 0-1.2 product:beyond0ltexx model:SM_G970F device:beyond0 transport_id:1\n\
            192.168.1.108:5555     device product:beyond0ltexx model:SM_G970F transport_id:3\n\
            emulator-5554          unauthorized transport_id:2\n\n";
        let list = parse_devices(out);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].serial, "R28M303RHET");
        assert_eq!(list[0].state, "device");
        assert_eq!(list[0].model.as_deref(), Some("SM G970F"));
        assert!(!list[0].network);
        assert!(list[1].network);
        assert_eq!(list[2].state, "unauthorized");
        assert_eq!(list[2].model, None);
    }

    #[test]
    fn parses_wlan_and_mdns() {
        let ip = "37: wlan0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc mq state UP group default qlen 3000\n    \
                  inet 192.168.1.108/24 brd 192.168.1.255 scope global wlan0\n       valid_lft forever";
        assert_eq!(parse_wlan_ip(ip), Some(Ipv4Addr::new(192, 168, 1, 108)));
        assert_eq!(parse_wlan_ip("Device \"wlan0\" does not exist."), None);
        let mdns = "List of discovered mdns services\n\
                    adb-R28M303RHET-AbCdEf\t_adb-tls-connect._tcp\t192.168.1.108:37123\n\
                    adb-R28M303RHET-AbCdEf\t_adb-tls-pairing._tcp\t192.168.1.108:41000\n";
        assert_eq!(
            parse_mdns_connect(mdns),
            vec![("adb-R28M303RHET-AbCdEf".to_string(), "192.168.1.108:37123".to_string())]
        );
    }
}
