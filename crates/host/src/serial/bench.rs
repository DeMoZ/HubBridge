//! Latency and throughput measurements over a [`SerialPort`]; nothing device-specific.
//!
//! - **echo** (`--bench-echo`): the device sends back what it receives (TX wired to RX, or a
//!   loopback fake): round-trip latency of small writes and the throughput of a long
//!   patterned stream, every byte checked.
//! - **request/response** (`--bench-request`): send fixed bytes repeatedly and time the
//!   reply of a device that answers them.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::port::{RxBuffer, SerialPort};
use super::{Line, Parity, StopBits};

/// Latency percentiles of a list of samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percentiles {
    pub p50: Duration,
    pub p95: Duration,
    pub max: Duration,
}

/// `None` for no samples.
pub fn percentiles(samples: &[Duration]) -> Option<Percentiles> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort();
    let at = |p: usize| v[((v.len() * p).div_ceil(100)).clamp(1, v.len()) - 1];
    Some(Percentiles {
        p50: at(50),
        p95: at(95),
        max: *v.last().unwrap(),
    })
}

/// Bytes per second the line can carry: start bit, data bits, parity bit, stop bits.
pub fn line_rate_bytes_per_s(line: &Line) -> f64 {
    let stop = match line.stop_bits {
        StopBits::One => 1.0,
        StopBits::OnePointFive => 1.5,
        StopBits::Two => 2.0,
    };
    let bits = 1.0 + line.data_bits as f64 + if line.parity == Parity::None { 0.0 } else { 1.0 } + stop;
    line.baud as f64 / bits
}

/// The byte at `i` of the test stream: not periodic over short spans, so shifts show.
pub fn pattern_byte(i: u64) -> u8 {
    (i.wrapping_mul(7).wrapping_add(i / 251)) as u8
}

async fn recv_within(rx: &RxBuffer, d: Duration) -> Option<Vec<u8>> {
    tokio::time::timeout(d, rx.recv()).await.ok().flatten()
}

/// Throw away what is buffered (late bytes of an earlier round).
async fn flush(port: &SerialPort) {
    let rx = port.rx();
    while recv_within(&rx, Duration::from_millis(30)).await.is_some() {}
    rx.clear();
}

#[derive(Debug, Clone)]
pub struct EchoOptions {
    /// Round trips for the latency figures.
    pub count: usize,
    /// Bytes per round trip.
    pub size: usize,
    /// Bytes of the throughput stream.
    pub bytes: usize,
    /// Bytes allowed unacknowledged during the stream (keeps the chip FIFO from overflowing).
    pub window: usize,
    /// Give up on a byte that does not come back after this long.
    pub timeout: Duration,
}

impl Default for EchoOptions {
    fn default() -> Self {
        EchoOptions {
            count: 200,
            size: 32,
            bytes: 1 << 20,
            window: 512,
            timeout: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EchoResult {
    pub rounds: usize,
    pub latency: Option<Percentiles>,
    pub stream_bytes: u64,
    pub elapsed: Duration,
    pub kb_per_s: f64,
    /// Share of the line rate the stream reached, in percent.
    pub line_rate_percent: f64,
    pub lost: u64,
    pub wrong: u64,
}

pub async fn bench_echo(port: &SerialPort, opts: &EchoOptions) -> Result<EchoResult> {
    if opts.window == 0 || opts.size == 0 {
        bail!("window and size must be more than 0");
    }
    let rx = port.rx();
    flush(port).await;
    let mut lost = 0u64;
    let mut wrong = 0u64;

    // Latency of small writes.
    let mut samples = Vec::new();
    let mut offset = 0u64;
    for _ in 0..opts.count {
        let msg: Vec<u8> = (0..opts.size as u64).map(|i| pattern_byte(offset + i)).collect();
        offset += opts.size as u64;
        let start = Instant::now();
        port.write(&msg).await?;
        let mut got: Vec<u8> = Vec::with_capacity(msg.len());
        let deadline = start + opts.timeout;
        while got.len() < msg.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            match recv_within(&rx, left).await {
                Some(c) => got.extend(c),
                None => break,
            }
        }
        if got.len() == msg.len() {
            samples.push(start.elapsed());
        }
        lost += (msg.len() - got.len().min(msg.len())) as u64;
        wrong += got.iter().zip(&msg).filter(|(a, b)| a != b).count() as u64;
        if got.len() < msg.len() {
            // Whatever comes late belongs to this round: do not mix it into the next.
            flush(port).await;
        }
    }

    // Throughput of a long stream with a bounded number of bytes in flight.
    let total = opts.bytes as u64;
    let (mut sent, mut received) = (0u64, 0u64);
    let start = Instant::now();
    while received < total {
        while sent < total && sent - received < opts.window as u64 {
            let n = (opts.window as u64 - (sent - received)).min(total - sent).min(4096) as usize;
            let chunk: Vec<u8> = (0..n as u64).map(|i| pattern_byte(sent + i)).collect();
            port.write(&chunk).await?;
            sent += n as u64;
        }
        match recv_within(&rx, opts.timeout).await {
            Some(c) => {
                for (i, b) in c.iter().enumerate() {
                    if *b != pattern_byte(received + i as u64) {
                        wrong += 1;
                    }
                }
                received += c.len() as u64;
            }
            None => {
                lost += sent - received;
                break;
            }
        }
    }
    let elapsed = start.elapsed();
    let rate = received as f64 / elapsed.as_secs_f64().max(1e-9);
    let line_rate = line_rate_bytes_per_s(&port.line());
    Ok(EchoResult {
        rounds: samples.len(),
        latency: percentiles(&samples),
        stream_bytes: received,
        elapsed,
        kb_per_s: rate / 1000.0,
        line_rate_percent: rate / line_rate * 100.0,
        lost,
        wrong,
    })
}

#[derive(Debug, Clone)]
pub struct RequestOptions {
    pub request: Vec<u8>,
    /// Complete when this many bytes arrived.
    pub reply_len: Option<usize>,
    /// Complete when the line was idle this long after the first byte.
    pub reply_idle: Option<Duration>,
    pub count: usize,
    pub interval: Duration,
    /// A round without a complete reply after this long counts as timed out.
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct RequestResult {
    pub rounds: usize,
    pub timeouts: usize,
    /// The reply of the first round that got one.
    pub first_reply: Option<Vec<u8>>,
    pub latency: Option<Percentiles>,
    pub reply_bytes: u64,
    pub elapsed: Duration,
    pub reply_bytes_per_s: f64,
}

pub async fn bench_request(port: &SerialPort, opts: &RequestOptions) -> Result<RequestResult> {
    if opts.request.is_empty() {
        bail!("the request is empty");
    }
    if opts.reply_len.is_none() && opts.reply_idle.is_none() {
        bail!("say when a reply is complete: --reply-len or --reply-idle-ms");
    }
    let rx = port.rx();
    flush(port).await;
    let mut samples = Vec::new();
    let mut timeouts = 0;
    let mut first_reply = None;
    let mut reply_bytes = 0u64;
    let started = Instant::now();
    for round in 0..opts.count {
        if round > 0 && !opts.interval.is_zero() {
            tokio::time::sleep(opts.interval).await;
        }
        // Late bytes of an earlier round are not this round's reply.
        rx.clear();
        let start = Instant::now();
        port.write(&opts.request).await?;
        let deadline = start + opts.timeout;
        let mut got: Vec<u8> = Vec::new();
        let complete = loop {
            if opts.reply_len.is_some_and(|n| got.len() >= n) {
                break true;
            }
            // With an idle rule, the wait after the first byte is the idle time.
            let limit = match (got.is_empty(), opts.reply_idle) {
                (false, Some(idle)) => idle.min(deadline.saturating_duration_since(Instant::now())),
                _ => deadline.saturating_duration_since(Instant::now()),
            };
            match recv_within(&rx, limit).await {
                Some(c) => got.extend(c),
                // Idle after the first byte is a complete reply (unless a length was demanded).
                None => break !got.is_empty() && opts.reply_len.is_none() && opts.reply_idle.is_some(),
            }
        };
        if complete {
            // The reply is complete when the last byte arrived, not when idleness was noticed.
            let latency = match opts.reply_idle {
                Some(idle) if opts.reply_len.is_none() => start.elapsed().saturating_sub(idle),
                _ => start.elapsed(),
            };
            samples.push(latency);
            reply_bytes += got.len() as u64;
            if first_reply.is_none() {
                first_reply = Some(match opts.reply_len {
                    Some(n) => got[..n.min(got.len())].to_vec(),
                    None => got,
                });
            }
        } else {
            timeouts += 1;
        }
    }
    let elapsed = started.elapsed();
    Ok(RequestResult {
        rounds: opts.count,
        timeouts,
        first_reply,
        latency: percentiles(&samples),
        reply_bytes,
        elapsed,
        reply_bytes_per_s: reply_bytes as f64 / elapsed.as_secs_f64().max(1e-9),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: &[u64]) -> Vec<Duration> {
        v.iter().map(|m| Duration::from_millis(*m)).collect()
    }

    #[test]
    fn percentile_picks() {
        let p = percentiles(&ms(&(1..=100).collect::<Vec<_>>())).unwrap();
        assert_eq!((p.p50, p.p95, p.max), (ms(&[50])[0], ms(&[95])[0], ms(&[100])[0]));
        let p = percentiles(&ms(&[7])).unwrap();
        assert_eq!((p.p50, p.p95, p.max), (ms(&[7])[0], ms(&[7])[0], ms(&[7])[0]));
        assert!(percentiles(&[]).is_none());
        // Unsorted input.
        let p = percentiles(&ms(&[9, 1, 5])).unwrap();
        assert_eq!((p.p50, p.max), (ms(&[5])[0], ms(&[9])[0]));
    }

    #[test]
    fn line_rate_of_8n1() {
        let r = line_rate_bytes_per_s(&Line::default());
        assert!((r - 11_520.0).abs() < 0.01, "{r}");
        let l = Line {
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            ..Line::default()
        };
        assert!((line_rate_bytes_per_s(&l) - 115_200.0 / 12.0).abs() < 0.01);
    }

    #[test]
    fn pattern_does_not_repeat_every_256_bytes() {
        let a: Vec<u8> = (0..256).map(pattern_byte).collect();
        let b: Vec<u8> = (256..512).map(pattern_byte).collect();
        assert_ne!(a, b);
    }
}
