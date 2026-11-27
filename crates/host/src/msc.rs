//! USB mass storage over HubBridge: Bulk-Only Transport + SCSI, read-only.
//!
//! First real consumer of the step 2 transfer path and a hardware-independent test of it:
//! any flash drive on the phone's hub works. `READ(10)` data is requested as several
//! transfers in flight, so the Wi-Fi round trip is paid once per command, not per packet.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hubbridge_proto::{Setup, UsbDeviceInfo, UsbStatus};

use crate::Client;

const CBW_SIG: &[u8; 4] = b"USBC";
const CSW_SIG: &[u8; 4] = b"USBS";
const TIMEOUT: Duration = Duration::from_secs(10);
/// Size of one IN transfer in a data phase.
pub const CHUNK: u32 = 64 * 1024;
/// Bytes per READ(10) in [`MassStorage::bench`].
pub const READ_COMMAND: u32 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inquiry {
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub removable: bool,
    pub device_type: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub blocks: u64,
    pub block_size: u32,
}

impl Capacity {
    pub fn bytes(&self) -> u64 {
        self.blocks * self.block_size as u64
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BenchResult {
    pub bytes: u64,
    pub elapsed: Duration,
}

impl BenchResult {
    pub fn mb_per_s(&self) -> f64 {
        self.bytes as f64 / 1e6 / self.elapsed.as_secs_f64().max(1e-9)
    }
}

pub struct MassStorage<'a> {
    client: &'a Client,
    id: u32,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
    tag: u32,
}

fn is_csw(d: &[u8], tag: u32) -> bool {
    d.len() == 13 && &d[..4] == CSW_SIG && d[4..8] == tag.to_le_bytes()
}

/// The Bulk-Only mass-storage interface of a device: (interface, bulk IN, bulk OUT).
pub fn find_interface(dev: &UsbDeviceInfo) -> Option<(u8, u8, u8)> {
    dev.configurations.iter().flat_map(|c| &c.interfaces).find_map(|i| {
        if i.class != 0x08 || i.protocol != 0x50 {
            return None;
        }
        let bulk = |dir_in: bool| {
            i.endpoints
                .iter()
                .find(|e| e.attributes & 3 == 2 && e.is_in() == dir_in)
                .map(|e| e.address)
        };
        Some((i.number, bulk(true)?, bulk(false)?))
    })
}

impl<'a> MassStorage<'a> {
    /// Open the device and claim its mass-storage interface (the phone unmounts it).
    pub async fn open(client: &'a Client, dev: &UsbDeviceInfo) -> Result<MassStorage<'a>> {
        let (interface, ep_in, ep_out) =
            find_interface(dev).context("not a USB mass-storage device (Bulk-Only, class 08 protocol 50)")?;
        client.open_device(dev.id).await?;
        if let Err(e) = client.claim_interface(dev.id, interface, true).await {
            let _ = client.close_device(dev.id).await;
            return Err(e);
        }
        let mut ms = MassStorage {
            client,
            id: dev.id,
            interface,
            ep_in,
            ep_out,
            tag: 1,
        };
        // A drive just taken from the phone often reports UNIT ATTENTION once.
        for _ in 0..3 {
            if ms.test_unit_ready().await.unwrap_or(false) {
                break;
            }
            let _ = ms.request_sense().await;
        }
        Ok(ms)
    }

    pub async fn close(self) -> Result<()> {
        let _ = self.client.release_interface(self.id, self.interface).await;
        self.client.close_device(self.id).await
    }

    async fn reset_recovery(&self) {
        let reset = Setup {
            request_type: 0x21,
            request: 0xff,
            value: 0,
            index: self.interface as u16,
            length: 0,
        };
        let _ = self.client.control(self.id, reset, Vec::new(), TIMEOUT).await;
        let _ = self.client.clear_halt(self.id, self.ep_in).await;
        let _ = self.client.clear_halt(self.id, self.ep_out).await;
    }

    /// One SCSI command: CBW, data-in phase of `data_len` bytes, CSW. Returns (data, status).
    pub async fn command(&mut self, cb: &[u8], data_len: u32) -> Result<(Vec<u8>, u8)> {
        let tag = self.tag;
        self.tag = self.tag.wrapping_add(1);
        let mut cbw = Vec::with_capacity(31);
        cbw.extend_from_slice(CBW_SIG);
        cbw.extend_from_slice(&tag.to_le_bytes());
        cbw.extend_from_slice(&data_len.to_le_bytes());
        cbw.push(if data_len > 0 { 0x80 } else { 0x00 });
        cbw.push(0); // LUN
        cbw.push(cb.len() as u8);
        let mut block = [0u8; 16];
        block[..cb.len()].copy_from_slice(cb);
        cbw.extend_from_slice(&block);

        let sent = self.client.transfer(self.id, self.ep_out, 0, cbw, TIMEOUT).await?;
        if sent.status != UsbStatus::Ok {
            self.reset_recovery().await;
            bail!("sending the SCSI command failed: {:?}", sent.status);
        }

        // Data phase: all chunks in flight at once, collected in order.
        let mut data = Vec::with_capacity(data_len as usize);
        // A drive that sends less data than asked may put its CSW into one of the chunks we
        // already queued (instead of stalling): recognise it there.
        let mut early_csw: Option<Vec<u8>> = None;
        if data_len > 0 {
            let mut pending = Vec::new();
            let mut left = data_len;
            while left > 0 {
                let n = left.min(CHUNK);
                pending.push(
                    self.client
                        .submit_transfer(self.id, self.ep_in, n, Vec::new(), TIMEOUT)
                        .await?,
                );
                left -= n;
            }
            let mut short = false;
            let mut stalled = false;
            for p in pending {
                let r = p.wait().await?;
                if short || stalled {
                    if early_csw.is_none() && r.status == UsbStatus::Ok && is_csw(&r.data, tag) {
                        early_csw = Some(r.data);
                    }
                    continue;
                }
                match r.status {
                    UsbStatus::Ok => {
                        short = (r.data.len() as u32) < CHUNK;
                        data.extend_from_slice(&r.data);
                    }
                    UsbStatus::Stall => stalled = true,
                    s => bail!("reading data failed: {s:?}"),
                }
            }
            if stalled {
                self.client.clear_halt(self.id, self.ep_in).await?;
            }
        }

        // Status phase.
        let csw = match early_csw {
            Some(c) => c,
            None => {
                let mut r = self
                    .client
                    .transfer(self.id, self.ep_in, 13, Vec::new(), TIMEOUT)
                    .await?;
                if r.status == UsbStatus::Stall {
                    self.client.clear_halt(self.id, self.ep_in).await?;
                    r = self
                        .client
                        .transfer(self.id, self.ep_in, 13, Vec::new(), TIMEOUT)
                        .await?;
                }
                r.ok()?
            }
        };
        if !is_csw(&csw, tag) {
            self.reset_recovery().await;
            bail!("bad status from the drive (CSW {} bytes)", csw.len());
        }
        let status = csw[12];
        if status == 2 {
            self.reset_recovery().await;
            bail!("the drive reported a phase error");
        }
        Ok((data, status))
    }

    pub async fn test_unit_ready(&mut self) -> Result<bool> {
        Ok(self.command(&[0x00, 0, 0, 0, 0, 0], 0).await?.1 == 0)
    }

    pub async fn request_sense(&mut self) -> Result<Vec<u8>> {
        Ok(self.command(&[0x03, 0, 0, 0, 18, 0], 18).await?.0)
    }

    pub async fn inquiry(&mut self) -> Result<Inquiry> {
        let (d, status) = self.command(&[0x12, 0, 0, 0, 36, 0], 36).await?;
        if status != 0 || d.len() < 36 {
            bail!("INQUIRY failed (status {status}, {} bytes)", d.len());
        }
        let text = |r: std::ops::Range<usize>| String::from_utf8_lossy(&d[r]).trim().to_string();
        Ok(Inquiry {
            device_type: d[0] & 0x1f,
            removable: d[1] & 0x80 != 0,
            vendor: text(8..16),
            product: text(16..32),
            revision: text(32..36),
        })
    }

    pub async fn read_capacity(&mut self) -> Result<Capacity> {
        let (d, status) = self.command(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8).await?;
        if status != 0 || d.len() < 8 {
            bail!("READ CAPACITY failed (status {status})");
        }
        let last = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as u64;
        let block_size = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
        Ok(Capacity {
            blocks: last + 1,
            block_size,
        })
    }

    /// READ(10): `count` blocks from `lba`.
    pub async fn read(&mut self, lba: u32, count: u16, block_size: u32) -> Result<Vec<u8>> {
        let mut cb = [0u8; 10];
        cb[0] = 0x28;
        cb[2..6].copy_from_slice(&lba.to_be_bytes());
        cb[7..9].copy_from_slice(&count.to_be_bytes());
        let len = count as u32 * block_size;
        let (d, status) = self.command(&cb, len).await?;
        if status != 0 || d.len() as u32 != len {
            bail!("READ(10) at {lba} failed (status {status}, {} of {len} bytes)", d.len());
        }
        Ok(d)
    }

    /// Sequential read of `bytes` from the start of the drive.
    pub async fn bench(&mut self, cap: Capacity, bytes: u64) -> Result<BenchResult> {
        let per_cmd = (READ_COMMAND / cap.block_size).max(1) as u16;
        let total_blocks = (bytes / cap.block_size as u64).min(cap.blocks);
        let start = Instant::now();
        let mut lba: u64 = 0;
        let mut read = 0u64;
        while lba < total_blocks {
            let n = (total_blocks - lba).min(per_cmd as u64) as u16;
            read += self.read(lba as u32, n, cap.block_size).await?.len() as u64;
            lba += n as u64;
        }
        Ok(BenchResult {
            bytes: read,
            elapsed: start.elapsed(),
        })
    }
}

/// Read the device's descriptors live over control transfers (not the agent's cache).
pub async fn read_descriptors(client: &Client, id: u32) -> Result<(Vec<u8>, Vec<(u8, String)>)> {
    let dev = client
        .control(id, Setup::get_descriptor(1, 0, 0, 18), Vec::new(), TIMEOUT)
        .await?
        .ok()?;
    if dev.len() < 18 {
        bail!("short device descriptor ({} bytes)", dev.len());
    }
    let head = client
        .control(id, Setup::get_descriptor(2, 0, 0, 9), Vec::new(), TIMEOUT)
        .await?
        .ok()?;
    if head.len() < 4 {
        bail!("short configuration descriptor");
    }
    let total = u16::from_le_bytes([head[2], head[3]]);
    let config = client
        .control(id, Setup::get_descriptor(2, 0, 0, total), Vec::new(), TIMEOUT)
        .await?
        .ok()?;
    let mut raw = dev.clone();
    raw.extend_from_slice(&config);

    let mut strings = Vec::new();
    let langs = client
        .control(id, Setup::get_descriptor(3, 0, 0, 255), Vec::new(), TIMEOUT)
        .await?;
    if langs.status == UsbStatus::Ok && langs.data.len() >= 4 {
        let lang = u16::from_le_bytes([langs.data[2], langs.data[3]]);
        for index in [dev[14], dev[15], dev[16]] {
            if index == 0 {
                continue;
            }
            let r = client
                .control(id, Setup::get_descriptor(3, index, lang, 255), Vec::new(), TIMEOUT)
                .await?;
            if r.status == UsbStatus::Ok && r.data.len() >= 2 {
                let units: Vec<u16> = r.data[2..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                strings.push((index, String::from_utf16_lossy(&units)));
            }
        }
    }
    Ok((raw, strings))
}
