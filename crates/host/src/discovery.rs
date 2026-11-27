//! Find agents on the local network (DNS-SD `_hubbridge._tcp`, registered by Android NSD).

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::Result;
use hubbridge_proto::MDNS_SERVICE_TYPE;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredAgent {
    /// From the TXT record `id`; empty if the agent did not publish it.
    pub agent_id: String,
    pub name: String,
    /// IPv4 first, then IPv6.
    pub addrs: Vec<SocketAddr>,
    pub proto_version: Option<u16>,
}

fn agent_from(info: &ResolvedService) -> DiscoveredAgent {
    let port = info.get_port();
    let mut addrs: Vec<SocketAddr> = info
        .get_addresses()
        .iter()
        .map(|ip| SocketAddr::new(ip.to_ip_addr(), port))
        .filter(|a| !a.ip().is_loopback())
        .filter(|a| !matches!(a.ip(), IpAddr::V6(v6) if v6.is_unicast_link_local()))
        .collect();
    addrs.sort_by_key(|a| (a.is_ipv6(), a.ip()));
    let instance = info.get_fullname().split('.').next().unwrap_or_default().to_string();
    DiscoveredAgent {
        agent_id: info.get_property_val_str("id").unwrap_or_default().to_string(),
        name: info
            .get_property_val_str("name")
            .map(|n| hubbridge_proto::usb::clean_str(n).to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or(instance),
        addrs,
        proto_version: info.get_property_val_str("v").and_then(|v| v.parse().ok()),
    }
}

/// Merge records of one agent seen under several service names / address sets.
fn merged(by_fullname: &BTreeMap<String, DiscoveredAgent>) -> Vec<DiscoveredAgent> {
    let mut out: Vec<DiscoveredAgent> = Vec::new();
    for agent in by_fullname.values() {
        match out
            .iter_mut()
            .find(|a| a.agent_id == agent.agent_id && a.name == agent.name)
        {
            Some(existing) => {
                for a in &agent.addrs {
                    if !existing.addrs.contains(a) {
                        existing.addrs.push(*a);
                    }
                }
            }
            None => out.push(agent.clone()),
        }
    }
    out
}

/// Browse for `timeout` and return every agent that resolved.
pub async fn discover(timeout: Duration) -> Result<Vec<DiscoveredAgent>> {
    tokio::task::spawn_blocking(move || discover_blocking(timeout)).await?
}

fn discover_blocking(timeout: Duration) -> Result<Vec<DiscoveredAgent>> {
    let daemon = ServiceDaemon::new()?;
    let rx = daemon.browse(MDNS_SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;
    let mut found = BTreeMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = rx.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(info) = event {
            found.insert(info.get_fullname().to_string(), agent_from(&info));
        }
    }
    let _ = daemon.shutdown();
    Ok(merged(&found))
}

/// Continuous browsing for the desktop window: calls `on_change` with the full list every
/// time an agent appears, changes or goes away. Stops when dropped.
pub struct Browser {
    daemon: ServiceDaemon,
}

impl Browser {
    pub fn start(on_change: impl Fn(Vec<DiscoveredAgent>) + Send + 'static) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let rx = daemon.browse(MDNS_SERVICE_TYPE)?;
        std::thread::Builder::new().name("mdns-browse".into()).spawn(move || {
            let mut found: BTreeMap<String, DiscoveredAgent> = BTreeMap::new();
            while let Ok(event) = rx.recv() {
                let changed = match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let agent = agent_from(&info);
                        found.insert(info.get_fullname().to_string(), agent.clone()) != Some(agent)
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => found.remove(&fullname).is_some(),
                    ServiceEvent::SearchStopped(_) => break,
                    _ => false,
                };
                if changed {
                    on_change(merged(&found));
                }
            }
        })?;
        Ok(Browser { daemon })
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}
