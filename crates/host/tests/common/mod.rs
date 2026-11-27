//! Helpers shared by the end-to-end tests.

#![allow(dead_code)]

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// A TCP proxy between the host and the agent for one connection. `pause()` stops reading
/// from the host's socket (so the host's socket buffers fill up) while the agent → host
/// direction keeps flowing; `resume()` starts reading again.
pub struct PausableProxy {
    addr: SocketAddr,
    paused: watch::Sender<bool>,
}

impl PausableProxy {
    /// Listen on `127.0.0.1:0` and forward the first connection to `agent`.
    pub async fn start(agent: SocketAddr) -> PausableProxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (paused, paused_rx) = watch::channel(false);
        tokio::spawn(async move {
            let Ok((host, _)) = listener.accept().await else { return };
            let Ok(agent) = TcpStream::connect(agent).await else {
                return;
            };
            host.set_nodelay(true).ok();
            agent.set_nodelay(true).ok();
            let (mut host_r, mut host_w) = host.into_split();
            let (mut agent_r, mut agent_w) = agent.into_split();
            // host -> agent, honouring the pause between reads
            let mut gate = paused_rx;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    if gate.wait_for(|p| !*p).await.is_err() {
                        break;
                    }
                    match host_r.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if agent_w.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = agent_w.shutdown().await;
            });
            // agent -> host
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match agent_r.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if host_w.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = host_w.shutdown().await;
            });
        });
        PausableProxy { addr, paused }
    }

    /// Address the host connects to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop reading from the host's socket.
    pub fn pause(&self) {
        self.paused.send_replace(true);
    }

    pub fn resume(&self) {
        self.paused.send_replace(false);
    }
}
