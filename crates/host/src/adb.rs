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

async fn adb(serial: Option<&str>, args: &[&str]) -> Result<String> {
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
        bail!(
            "adb {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Serials of devices in the `device` state.
pub async fn devices() -> Result<Vec<String>> {
    let out = adb(None, &["devices"]).await?;
    Ok(out
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            match (it.next(), it.next()) {
                (Some(s), Some("device")) => Some(s.to_string()),
                _ => None,
            }
        })
        .collect())
}

/// Forward a local port to the agent port on the phone; reuses an existing forward.
/// Returns the local address to connect to.
pub async fn forward(serial: Option<&str>, agent_port: u16) -> Result<SocketAddr> {
    let serial = match serial {
        Some(s) => s.to_string(),
        None => {
            let list = devices().await?;
            match list.len() {
                0 => bail!("no device in `adb devices`: connect the phone and allow USB debugging"),
                1 => list[0].clone(),
                _ => bail!("several adb devices ({}): pick one with adb:<serial>", list.join(", ")),
            }
        }
    };
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
