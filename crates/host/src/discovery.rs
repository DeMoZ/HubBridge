//! Find agents on the local network (DNS-SD `_hubbridge._tcp`, registered by Android NSD).

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::Result;
use hubbridge_proto::MDNS_SERVICE_TYPE;
use mdns_sd::{ServiceDaemon, ServiceEvent};

#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    /// From the TXT record `id`; empty if the agent did not publish it.
    pub agent_id: String,
    pub name: String,
    /// IPv4 first, then IPv6.
    pub addrs: Vec<SocketAddr>,
    pub proto_version: Option<u16>,
}

/// Browse for `timeout` and return every agent that resolved.
pub async fn discover(timeout: Duration) -> Result<Vec<DiscoveredAgent>> {
    tokio::task::spawn_blocking(move || discover_blocking(timeout)).await?
}

fn discover_blocking(timeout: Duration) -> Result<Vec<DiscoveredAgent>> {
    let daemon = ServiceDaemon::new()?;
    let rx = daemon.browse(MDNS_SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<DiscoveredAgent> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = rx.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(info) = event {
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
            let agent = DiscoveredAgent {
                agent_id: info.get_property_val_str("id").unwrap_or_default().to_string(),
                name: info
                    .get_property_val_str("name")
                    .map(str::to_string)
                    .unwrap_or(instance),
                addrs,
                proto_version: info.get_property_val_str("v").and_then(|v| v.parse().ok()),
            };
            match found
                .iter_mut()
                .find(|a| a.agent_id == agent.agent_id && a.name == agent.name)
            {
                Some(existing) => {
                    for a in agent.addrs {
                        if !existing.addrs.contains(&a) {
                            existing.addrs.push(a);
                        }
                    }
                }
                None => found.push(agent),
            }
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}
