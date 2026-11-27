//! Pseudo-terminal front end (macOS and Linux): the serial port as `/dev/ttysNNN`.
//!
//! The master side is served here, the slave fd is **held open** for the bridge's lifetime:
//! the master then never sees a hang-up when an application closes the slave, applications
//! can reopen it, and the slave's termios stays readable (on Linux the master has its own).
//! `TIOCPKT` mode on the master tells flushes (`tcflush`) apart from data. A watcher reads
//! the slave's termios and turns changes of speed, size, parity, stop bits and RTS/CTS into
//! line changes on the port, before the data the application wrote after the change.

// `tcflag_t` is u64 on macOS and u32 on Linux: the casts to u64 are needed on one of them.
#![allow(clippy::unnecessary_cast)]

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl, open};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{ControlModes, OptionalActions, Termios, tcgetattr, tcsetattr};
use tokio::io::unix::AsyncFd;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::port::RxBuffer;
use super::{Flow, Line, Parity, SerialPort, StopBits};

const TIOCPKT_FLUSHREAD: u8 = 0x01;
const TIOCPKT_FLUSHWRITE: u8 = 0x02;
/// How often the slave's termios is looked at when no data triggers a look.
pub const WATCH_PERIOD: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, Default)]
pub struct PtyOptions {
    /// Ignore the line settings applications set on the PTY.
    pub lock_line: bool,
}

pub struct PtyFront {
    path: String,
    tasks: Vec<JoinHandle<()>>,
    ended: tokio::sync::watch::Receiver<bool>,
}

/// Tells the watchers when a task ends, however it ends (also when aborted).
struct EndGuard(Arc<tokio::sync::watch::Sender<bool>>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// What an application can set on the slave that matters to a UART.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub line: Line,
    pub flow: Flow,
}

fn bits(t: &Termios) -> u64 {
    t.control_modes.bits() as u64
}

/// The UART settings in a termios.
pub fn observe(t: &Termios) -> Observed {
    let c = bits(t);
    let has = |flag: libc::tcflag_t| c & (flag as u64) != 0;
    let size = c & (libc::CSIZE as u64);
    let data_bits = if size == libc::CS5 as u64 {
        5
    } else if size == libc::CS6 as u64 {
        6
    } else if size == libc::CS7 as u64 {
        7
    } else {
        8
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let stick = has(libc::CMSPAR);
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let stick = false;
    let parity = if !has(libc::PARENB) {
        Parity::None
    } else {
        match (stick, has(libc::PARODD)) {
            (true, true) => Parity::Mark,
            (true, false) => Parity::Space,
            (false, true) => Parity::Odd,
            (false, false) => Parity::Even,
        }
    };
    Observed {
        line: Line {
            baud: t.output_speed(),
            data_bits,
            parity,
            stop_bits: if has(libc::CSTOPB) {
                StopBits::Two
            } else {
                StopBits::One
            },
        },
        flow: if has(libc::CRTSCTS) { Flow::RtsCts } else { Flow::None },
    }
}

/// Raw mode with the UART settings of `obs`.
pub(crate) fn configure(t: &mut Termios, obs: &Observed) -> Result<()> {
    t.make_raw();
    let mut c = bits(t)
        & !(libc::CSIZE as u64
            | libc::PARENB as u64
            | libc::PARODD as u64
            | libc::CSTOPB as u64
            | libc::CRTSCTS as u64);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        c &= !(libc::CMSPAR as u64);
    }
    c |= match obs.line.data_bits {
        5 => libc::CS5,
        6 => libc::CS6,
        7 => libc::CS7,
        _ => libc::CS8,
    } as u64;
    if obs.line.parity != Parity::None {
        c |= libc::PARENB as u64;
        if matches!(obs.line.parity, Parity::Odd | Parity::Mark) {
            c |= libc::PARODD as u64;
        }
        // Mark and space are "stick" parity: odd+stick is mark, even+stick is space.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if matches!(obs.line.parity, Parity::Mark | Parity::Space) {
            c |= libc::CMSPAR as u64;
        }
    }
    if obs.flow == Flow::RtsCts {
        c |= libc::CRTSCTS as u64;
    }
    if obs.line.stop_bits == StopBits::Two {
        c |= libc::CSTOPB as u64;
    }
    t.control_modes = ControlModes::from_bits_retain(c as _);
    t.set_speed(obs.line.baud)
        .map_err(|e| anyhow!("set the PTY speed to {}: {e}", obs.line.baud))?;
    Ok(())
}

impl PtyFront {
    /// Create the pseudo-terminal for `port` and start serving it.
    pub fn start(port: SerialPort, opts: PtyOptions) -> Result<PtyFront> {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).context("open a pseudo-terminal")?;
        grantpt(&master).context("grantpt")?;
        unlockpt(&master).context("unlockpt")?;
        let name = ptsname(&master, Vec::new()).context("ptsname")?;
        let path = name.to_string_lossy().into_owned();
        let flags = fcntl_getfl(&master)?;
        fcntl_setfl(&master, flags | OFlags::NONBLOCK)?;
        // Hold the slave open (raw mode, the port's current line).
        let slave = open(&name, OFlags::RDWR | OFlags::NOCTTY, Mode::empty()).context("open the PTY slave")?;
        let initial = Observed {
            line: port.line(),
            flow: port.flow(),
        };
        let mut t = tcgetattr(&slave)?;
        configure(&mut t, &initial)?;
        tcsetattr(&slave, OptionalActions::Now, &t).context("set the PTY slave to raw mode")?;
        set_packet_mode(&master)?;

        // `AsyncFd::new` is deprecated for I/O-safety reasons in new tokio versions, but is the
        // safe constructor of the versions this workspace may resolve to; the fd is an owned
        // pty master that stays registered for its whole life.
        #[allow(deprecated)]
        let afd = Arc::new(AsyncFd::new(master).context("register the PTY master")?);
        let slave = Arc::new(slave);
        // Either direction ending ends the front end.
        let (end_tx, ended) = tokio::sync::watch::channel(false);
        let end_tx = Arc::new(end_tx);
        let (g1, g2) = (EndGuard(Arc::clone(&end_tx)), EndGuard(end_tx));
        let observed = observe(&tcgetattr(&*slave)?);
        let reader = {
            let (afd, slave, port) = (Arc::clone(&afd), Arc::clone(&slave), port.clone());
            tokio::spawn(async move {
                let _end = g1;
                master_reader(afd, slave, port, opts, observed).await;
            })
        };
        let rx = port.tap();
        let writer = tokio::spawn(async move {
            let _end = g2;
            master_writer(afd, slave, rx).await;
        });
        Ok(PtyFront {
            path,
            tasks: vec![reader, writer],
            ended,
        })
    }

    /// The path applications open, e.g. `/dev/ttys004`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Stop serving and close the pseudo-terminal.
    pub fn stop(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }

    /// Resolves when a direction has ended (port closed, an I/O error, stopped).
    pub fn ended(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.ended.clone();
        async move {
            let _ = rx.wait_for(|v| *v).await;
        }
    }
}

impl Drop for PtyFront {
    fn drop(&mut self) {
        self.stop();
    }
}

fn set_packet_mode(master: &OwnedFd) -> Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: TIOCPKT takes a pointer to an int; the fd is a valid pty master.
    let r = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCPKT as _, &on as *const libc::c_int) };
    if r != 0 {
        return Err(anyhow!("TIOCPKT: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

async fn read_master(afd: &AsyncFd<OwnedFd>, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let mut guard = afd.readable().await?;
        match guard.try_io(|a| rustix::io::read(a.get_ref(), &mut *buf).map_err(std::io::Error::from)) {
            Ok(r) => return r,
            Err(_would_block) => continue,
        }
    }
}

async fn write_master(afd: &AsyncFd<OwnedFd>, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let mut guard = afd.writable().await?;
        match guard.try_io(|a| rustix::io::write(a.get_ref(), data).map_err(std::io::Error::from)) {
            Ok(Ok(n)) => data = &data[n..],
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

/// Look at the slave's termios and apply a change to the port.
async fn sync_line(slave: &OwnedFd, port: &SerialPort, last: &mut Observed) {
    let Ok(t) = tcgetattr(slave) else { return };
    let now = observe(&t);
    if now == *last {
        return;
    }
    let prev = *last;
    *last = now;
    // A speed of 0 means "hang up" to a tty: nothing to program.
    if now.line != prev.line && now.line.baud != 0 {
        match port.set_line(now.line).await {
            Ok(eff) => debug!(line = %eff, "PTY line change applied"),
            Err(e) => warn!("PTY line change to {} refused: {e:#}", now.line),
        }
    }
    if now.flow != prev.flow
        && let Err(e) = port.set_flow(now.flow).await
    {
        warn!("PTY flow control change refused: {e:#}");
    }
}

/// Application → device.
async fn master_reader(
    afd: Arc<AsyncFd<OwnedFd>>,
    slave: Arc<OwnedFd>,
    port: SerialPort,
    opts: PtyOptions,
    mut last: Observed,
) {
    let done = port.done();
    tokio::pin!(done);
    let mut tick = tokio::time::interval(WATCH_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = vec![0u8; 16 * 1024 + 1];
    loop {
        tokio::select! {
            r = read_master(&afd, &mut buf) => {
                let n = match r {
                    Ok(n) if n > 0 => n,
                    Ok(_) => continue,
                    Err(e) => {
                        warn!("PTY read failed: {e}");
                        return;
                    }
                };
                if buf[0] == 0 {
                    // The application may have changed the line just before writing.
                    if !opts.lock_line {
                        sync_line(&slave, &port, &mut last).await;
                    }
                    if port.write(&buf[1..n]).await.is_err() {
                        return;
                    }
                } else {
                    let flags = buf[0];
                    let rx = flags & TIOCPKT_FLUSHREAD != 0;
                    let tx = flags & TIOCPKT_FLUSHWRITE != 0;
                    if (rx || tx) && let Err(e) = port.purge(rx, tx).await {
                        warn!("PTY flush could not purge the port: {e:#}");
                    }
                }
            }
            _ = tick.tick(), if !opts.lock_line => sync_line(&slave, &port, &mut last).await,
            _ = &mut done => return,
        }
    }
}

/// Device → application.
async fn master_writer(afd: Arc<AsyncFd<OwnedFd>>, _slave: Arc<OwnedFd>, rx: Arc<RxBuffer>) {
    while let Some(chunk) = rx.recv().await {
        if let Err(e) = write_master(&afd, &chunk).await {
            warn!("PTY write failed: {e}");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh slave termios from a real pty.
    fn fresh_termios() -> Termios {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = open(&name, OFlags::RDWR | OFlags::NOCTTY, Mode::empty()).unwrap();
        tcgetattr(&slave).unwrap()
    }

    fn obs(parity: Parity, stop_bits: StopBits, data_bits: u8, flow: Flow, baud: u32) -> Observed {
        Observed {
            line: Line {
                baud,
                data_bits,
                parity,
                stop_bits,
            },
            flow,
        }
    }

    #[test]
    fn configure_and_observe_round_trip() {
        let mut cases = vec![
            obs(Parity::None, StopBits::One, 8, Flow::None, 115_200),
            obs(Parity::Even, StopBits::Two, 7, Flow::None, 9600),
            obs(Parity::Odd, StopBits::One, 6, Flow::RtsCts, 57_600),
            obs(Parity::None, StopBits::One, 5, Flow::RtsCts, 230_400),
        ];
        // Stick parity exists on Linux only.
        if cfg!(any(target_os = "linux", target_os = "android")) {
            cases.push(obs(Parity::Mark, StopBits::One, 8, Flow::None, 9600));
            cases.push(obs(Parity::Space, StopBits::Two, 8, Flow::RtsCts, 9600));
        }
        for want in cases {
            // Start from a state that has everything set, to see that it gets cleared too.
            let mut t = fresh_termios();
            configure(&mut t, &obs(Parity::Odd, StopBits::Two, 7, Flow::RtsCts, 4800)).unwrap();
            configure(&mut t, &want).unwrap();
            assert_eq!(observe(&t), want);
        }
    }

    #[test]
    fn flow_control_flag_follows_the_observation() {
        let mut t = fresh_termios();
        configure(&mut t, &obs(Parity::None, StopBits::One, 8, Flow::RtsCts, 9600)).unwrap();
        assert_eq!(observe(&t).flow, Flow::RtsCts);
        configure(&mut t, &obs(Parity::None, StopBits::One, 8, Flow::None, 9600)).unwrap();
        assert_eq!(observe(&t).flow, Flow::None);
    }
}
