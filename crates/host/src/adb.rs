//! Wired mode through ADB: `adb forward tcp:<local> tcp:<agent port>`.
//!
//! Works when the phone is connected to this computer by USB (or by wireless debugging).
//! Note: a phone with one USB-C port that hosts the hub cannot be connected to the computer
//! by USB at the same time — see docs/LIMITATIONS.md.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// `$ADB`, then `adb` on PATH, then the SDK's platform-tools.
pub fn adb_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ADB") {
        return PathBuf::from(p);
    }
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(exe);
            if p.is_file() {
                return p;
            }
        }
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(sdk) = std::env::var_os(var) {
            let p = PathBuf::from(sdk).join("platform-tools").join(exe);
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from(exe)
}

/// Run adb; on failure the error carries adb's stderr (and stdout, where `install` reports).
pub(crate) async fn adb(serial: Option<&str>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(adb_path());
    if let Some(s) = serial {
        cmd.args(["-s", s]);
    }
    let out = cmd
        .args(args)
        .output()
        .await
        .context("run adb (is it installed? set $ADB)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("adb {}: {msg}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One line of `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbDevice {
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, ...
    pub state: String,
    /// `model:` field with `_` turned back into spaces, e.g. "SM G970F".
    pub model: Option<String>,
}

impl AdbDevice {
    pub fn label(&self) -> String {
        match &self.model {
            Some(m) => format!("{m} ({})", self.serial),
            None => self.serial.clone(),
        }
    }
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
            Some(AdbDevice { serial, state, model })
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

fn state_hint(state: &str) -> &'static str {
    match state {
        "unauthorized" => "unlock the phone and allow USB debugging for this computer",
        "offline" => "reconnect the cable or restart adb (`adb kill-server`)",
        "no" | "no permissions" => "adb has no permission to open the device (Linux: udev rules)",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_l() {
        let out = "List of devices attached\n\
            R28M303RHET            device 0-1.2 product:beyond0ltexx model:SM_G970F device:beyond0 transport_id:1\n\
            emulator-5554          unauthorized transport_id:2\n\n";
        let list = parse_devices(out);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].serial, "R28M303RHET");
        assert_eq!(list[0].state, "device");
        assert_eq!(list[0].model.as_deref(), Some("SM G970F"));
        assert_eq!(list[1].state, "unauthorized");
        assert_eq!(list[1].model, None);
    }
}
