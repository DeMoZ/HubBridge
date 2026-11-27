//! A serial bridge: the port plus the chosen front ends, started and stopped together.
//!
//! It ends on [`BridgeHandle::stop`], on a port failure (device closed, unplugged, session
//! lost) or when a front end fails, and always closes the port. The library does not ping:
//! the desktop pings every second and the CLI every 2 s (the agent's idle timeout is 20 s).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, bail};
use hubbridge_proto::UsbDeviceInfo;
use tokio::task::JoinHandle;

use super::port::{PortInfo, PortOptions, SerialPort};
use super::tcp::{TcpFront, TcpOptions};
use super::{Flow, Line, SerialConfig, Stats};
use crate::Client;

#[cfg(unix)]
use super::pty::{PtyFront, PtyOptions};

#[derive(Debug, Clone)]
pub struct BridgeOptions {
    pub config: SerialConfig,
    pub port: PortOptions,
    /// A pseudo-terminal (macOS, Linux).
    pub pty: bool,
    /// Listen for TCP clients here.
    pub tcp: Option<SocketAddr>,
    /// With `tcp`: RFC 2217 instead of a raw stream.
    pub rfc2217: bool,
    /// Ignore line settings applications set (PTY termios, RFC 2217).
    pub lock_line: bool,
}

// Not derivable: the PTY is on by default only where there is one (`cfg!(unix)`), and clippy
// on Windows sees that as the plain `false` default.
#[allow(clippy::derivable_impls)]
impl Default for BridgeOptions {
    fn default() -> Self {
        BridgeOptions {
            config: SerialConfig::default(),
            port: PortOptions::default(),
            pty: cfg!(unix),
            tcp: None,
            rfc2217: false,
            lock_line: false,
        }
    }
}

/// What a running bridge offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeInfo {
    pub driver: &'static str,
    pub chip: Option<String>,
    /// `FTDI (FT232R)`.
    pub label: String,
    pub line: Line,
    pub flow: Flow,
    pub pty_path: Option<String>,
    pub tcp_addr: Option<SocketAddr>,
    pub rfc2217: bool,
    /// Set when the TCP listener is not loopback-only.
    pub warning: Option<String>,
}

pub struct SerialBridge;

pub struct BridgeHandle {
    port: SerialPort,
    #[cfg(unix)]
    pty: Option<PtyFront>,
    tcp: Option<TcpFront>,
    rfc2217: bool,
    /// Closes the port when the first thing ends.
    _supervisor: JoinHandle<()>,
}

impl SerialBridge {
    /// Open the device and start the front ends. The device is closed again on failure.
    pub async fn start(client: Arc<Client>, info: &UsbDeviceInfo, opts: BridgeOptions) -> Result<BridgeHandle> {
        if opts.pty && !cfg!(unix) {
            bail!("a pseudo-terminal is not available on this platform: use --tcp (with --rfc2217 for line control)");
        }
        if !opts.pty && opts.tcp.is_none() {
            bail!("nothing to serve: ask for a PTY or a TCP port");
        }
        let port = SerialPort::open(client, info, opts.config, opts.port.clone()).await?;
        let label = format!("{} {}", info.label(), port.info().label);
        #[cfg(unix)]
        let pty = if opts.pty {
            match PtyFront::start(
                port.clone(),
                PtyOptions {
                    lock_line: opts.lock_line,
                },
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    port.close().await;
                    return Err(e);
                }
            }
        } else {
            None
        };
        let tcp = match opts.tcp {
            Some(addr) => {
                match TcpFront::start(
                    port.clone(),
                    addr,
                    TcpOptions {
                        rfc2217: opts.rfc2217,
                        lock_line: opts.lock_line,
                        label,
                    },
                )
                .await
                {
                    Ok(t) => Some(t),
                    Err(e) => {
                        #[cfg(unix)]
                        if let Some(p) = &pty {
                            p.stop();
                        }
                        port.close().await;
                        return Err(e);
                    }
                }
            }
            None => None,
        };
        // Whatever ends first closes the port, so the device is given back: the port itself,
        // or a front end whose listener or terminal failed.
        let supervisor = {
            let port = port.clone();
            #[cfg(unix)]
            let pty_ended = pty.as_ref().map(|p| p.ended());
            let tcp_ended = tcp.as_ref().map(|t| t.ended());
            tokio::spawn(async move {
                #[cfg(unix)]
                let pty_end = async {
                    match pty_ended {
                        Some(f) => f.await,
                        None => std::future::pending().await,
                    }
                };
                #[cfg(not(unix))]
                let pty_end = std::future::pending::<()>();
                let tcp_end = async {
                    match tcp_ended {
                        Some(f) => f.await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = port.done() => {}
                    _ = pty_end => port.fail("the PTY front end ended".into()),
                    _ = tcp_end => port.fail("the TCP front end ended".into()),
                }
                port.close().await;
            })
        };
        Ok(BridgeHandle {
            port,
            #[cfg(unix)]
            pty,
            tcp,
            rfc2217: opts.rfc2217,
            _supervisor: supervisor,
        })
    }
}

impl BridgeHandle {
    pub fn port(&self) -> &SerialPort {
        &self.port
    }

    pub fn info(&self) -> BridgeInfo {
        let p: PortInfo = self.port.info();
        #[cfg(unix)]
        let pty_path = self.pty.as_ref().map(|p| p.path().to_string());
        #[cfg(not(unix))]
        let pty_path = None;
        BridgeInfo {
            driver: p.driver,
            chip: p.chip,
            label: p.label,
            line: p.line,
            flow: p.flow,
            pty_path,
            tcp_addr: self.tcp.as_ref().map(|t| t.addr()),
            rfc2217: self.rfc2217,
            warning: self.tcp.as_ref().and_then(|t| t.warning()).map(str::to_string),
        }
    }

    pub fn stats(&self) -> Stats {
        self.port.stats()
    }

    /// Resolves with the reason once the bridge has ended.
    pub async fn done(&self) -> String {
        self.port.done().await
    }

    /// Stop the front ends; the bridge then ends and closes the port by itself.
    pub fn stop_fronts(&self) {
        #[cfg(unix)]
        if let Some(p) = &self.pty {
            p.stop();
        }
        if let Some(t) = &self.tcp {
            t.stop();
        }
    }

    /// Stop the front ends and close the port (the device is given back).
    pub async fn stop(&self) {
        self.stop_fronts();
        self.port.close().await;
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(p) = &self.pty {
            p.stop();
        }
        if let Some(t) = &self.tcp {
            t.stop();
        }
        // Dropped without `stop`: still give the device back.
        let port = self.port.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { port.close().await });
        }
    }
}
