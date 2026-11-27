//! HubBridge desktop host: connects to the Android agent over Wi-Fi/LAN or `adb forward`.
//!
//! Step 1 scope: discovery, PIN pairing, encrypted session, agent info, USB device list
//! with descriptors, hot-plug events, RTT. USB data forwarding (step 2) and video (step 3)
//! build on [`client::Client`].

pub mod adb;
pub mod client;
pub mod discovery;
pub mod format;
pub mod install;
pub mod store;

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hubbridge_proto::DEFAULT_PORT;

pub use client::{Client, ClientEvent};
pub use store::Store;

/// How long name/id lookups browse mDNS.
pub const RESOLVE_BROWSE: Duration = Duration::from_millis(2500);

/// Turn a user-given target into addresses to try, best first.
///
/// - `adb` or `adb:<serial>` — set up (or reuse) `adb forward` to the agent port;
/// - `192.168.1.20`, `192.168.1.20:47800`, `phone.local:47800` — direct address;
/// - an agent name or id prefix — mDNS, then the last address that worked;
/// - nothing — the only paired agent, found the same way.
pub async fn resolve(target: Option<&str>, store: &Store) -> Result<Vec<SocketAddr>> {
    let target = match target {
        Some(t) => t.trim().to_string(),
        None => match store.data.agents.len() {
            1 => store.data.agents.keys().next().cloned().unwrap_or_default(),
            0 => bail!("no paired agent yet: run `hubbridge discover`, then `hubbridge pair <target> --pin <PIN>`"),
            _ => bail!("several paired agents: name one (see `hubbridge agents`)"),
        },
    };
    if target == "adb" {
        return Ok(vec![adb::forward(None, DEFAULT_PORT).await?]);
    }
    if let Some(serial) = target.strip_prefix("adb:") {
        return Ok(vec![adb::forward(Some(serial), DEFAULT_PORT).await?]);
    }
    if let Ok(addr) = target.parse::<SocketAddr>() {
        return Ok(vec![addr]);
    }
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, DEFAULT_PORT)]);
    }
    if target.contains(':') {
        let list: Vec<_> = tokio::net::lookup_host(&target)
            .await
            .with_context(|| format!("resolve {target}"))?
            .collect();
        if list.is_empty() {
            bail!("{target} has no address");
        }
        return Ok(list);
    }

    // Name or id: ask the network first, it has the current IP.
    let q = target.to_lowercase();
    let paired = store.find(&target)?;
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for a in discovery::discover(RESOLVE_BROWSE).await? {
        let by_id =
            !a.agent_id.is_empty() && (a.agent_id.starts_with(&q) || paired.is_some_and(|(id, _)| *id == a.agent_id));
        if by_id || a.name.trim().to_lowercase() == q {
            addrs.extend(a.addrs);
        }
    }
    if let Some((_, agent)) = paired
        && let Some(addr) = agent.last_addr.as_deref().and_then(|a| a.parse().ok())
        && !addrs.contains(&addr)
    {
        addrs.push(addr);
    }
    if addrs.is_empty()
        && let Ok(it) = tokio::net::lookup_host((target.as_str(), DEFAULT_PORT)).await
    {
        // Maybe a plain host name.
        addrs.extend(it);
    }
    // IPv4 first: phones on guest/"client isolation" Wi-Fi often have broken IPv6 routes.
    addrs.sort_by_key(|a| a.is_ipv6());
    if !addrs.is_empty() {
        return Ok(addrs);
    }
    bail!("cannot find agent '{target}' (not on mDNS, no saved address)")
}
