//! Real USB on Android through `nusb` on a usbfs file descriptor.
//!
//! Android gives the app an fd for each device it opened with
//! `UsbManager.openDevice()`; the platform layer passes a duplicate here. Each endpoint gets
//! a worker task that keeps several transfers in flight (usbfs URBs, completed in order),
//! which hides most of the network round trip for streaming endpoints.

use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hubbridge_proto::{Setup, UsbStatus};
use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, BulkOrInterrupt, Completion, ControlIn, ControlOut, ControlType, EndpointDirection, EndpointType, In,
    Interrupt, Out, Recipient, TransferError,
};
use nusb::{Device, Endpoint, Interface};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::usb::{BoxFut, TransferResult, UsbBackend, UsbDevice, UsbError};

/// Opens a device by id and returns its usbfs fd (the platform keeps the connection).
pub type Opener = Arc<dyn Fn(u32) -> Result<OwnedFd, UsbError> + Send + Sync>;
/// Called when the host closes the device, so the platform can close its connection.
pub type Closer = Arc<dyn Fn(u32) + Send + Sync>;

pub struct NusbBackend {
    opener: Opener,
    closer: Closer,
}

impl NusbBackend {
    pub fn new(opener: Opener, closer: Closer) -> Arc<Self> {
        Arc::new(NusbBackend { opener, closer })
    }
}

impl UsbBackend for NusbBackend {
    fn open(&self, id: u32) -> BoxFut<Result<Arc<dyn UsbDevice>, UsbError>> {
        let fd = (self.opener)(id);
        let closer = Arc::clone(&self.closer);
        Box::pin(async move {
            let fd = fd?;
            let device = match Device::from_fd(fd).await {
                Ok(d) => d,
                Err(e) => {
                    closer(id);
                    return Err(UsbError::Failed(format!("open usbfs fd: {e}")));
                }
            };
            Ok(Arc::new(NusbDevice {
                id,
                device,
                state: Arc::new(Mutex::new(DevState::default())),
                closer,
            }) as Arc<dyn UsbDevice>)
        })
    }
}

enum Job {
    Transfer {
        buf: Buffer,
        want: usize,
        timeout: Duration,
        reply: oneshot::Sender<TransferResult>,
    },
    ClearHalt {
        reply: oneshot::Sender<Result<(), UsbError>>,
    },
}

#[derive(Default)]
struct DevState {
    interfaces: HashMap<u8, Interface>,
    /// Endpoint address → (owning interface, worker queue).
    endpoints: HashMap<u8, (u8, mpsc::UnboundedSender<Job>)>,
}

pub struct NusbDevice {
    id: u32,
    device: Device,
    state: Arc<Mutex<DevState>>,
    closer: Closer,
}

fn status(e: TransferError) -> UsbStatus {
    match e {
        TransferError::Cancelled => UsbStatus::Cancelled,
        TransferError::Stall => UsbStatus::Stall,
        TransferError::Disconnected => UsbStatus::Disconnected,
        TransferError::Fault => UsbStatus::Fault,
        TransferError::InvalidArgument => UsbStatus::InvalidArgument,
        _ => UsbStatus::Unknown,
    }
}

fn completion(c: Completion, is_in: bool, want: usize) -> TransferResult {
    let actual = c.actual_len;
    let st = match c.status {
        Ok(()) => UsbStatus::Ok,
        Err(e) => status(e),
    };
    let data = if is_in {
        let mut v = c.buffer.into_vec();
        v.truncate(actual.min(want));
        v
    } else {
        Vec::new()
    };
    TransferResult {
        status: st,
        actual: if is_in { data.len() as u32 } else { actual as u32 },
        data,
    }
}

/// Owns one endpoint; keeps submitted transfers in order, applies timeouts.
fn spawn_worker<T: EndpointType + BulkOrInterrupt, D: EndpointDirection>(
    mut ep: Endpoint<T, D>,
    is_in: bool,
) -> mpsc::UnboundedSender<Job>
where
    Endpoint<T, D>: Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
    tokio::spawn(async move {
        let far = Duration::from_secs(3600 * 24);
        let mut pending: VecDeque<(oneshot::Sender<TransferResult>, Instant, usize)> = VecDeque::new();
        loop {
            let deadline = pending.front().map(|p| p.1).unwrap_or_else(|| Instant::now() + far);
            tokio::select! {
                job = rx.recv() => match job {
                    None => {
                        ep.cancel_all();
                        while ep.pending() > 0 {
                            let _ = ep.next_complete().await;
                        }
                        return;
                    }
                    Some(Job::Transfer { buf, want, timeout, reply }) => {
                        ep.submit(buf);
                        pending.push_back((reply, Instant::now() + timeout, want));
                    }
                    Some(Job::ClearHalt { reply }) => {
                        // Finish what is in flight first: a halt is cleared between transfers.
                        while let Some((r, _, want)) = pending.pop_front() {
                            let c = ep.next_complete().await;
                            let _ = r.send(completion(c, is_in, want));
                        }
                        let res = ep.clear_halt().await.map_err(|e| UsbError::Failed(format!("clear halt: {e}")));
                        let _ = reply.send(res);
                    }
                },
                c = ep.next_complete(), if !pending.is_empty() => {
                    if let Some((reply, _, want)) = pending.pop_front() {
                        let _ = reply.send(completion(c, is_in, want));
                    }
                }
                _ = tokio::time::sleep_until(deadline), if !pending.is_empty() => {
                    // usbfs has no per-URB timeout: cancel; the transfers come back Cancelled.
                    ep.cancel_all();
                    for p in pending.iter_mut() {
                        p.1 = Instant::now() + far;
                    }
                }
            }
        }
    });
    tx
}

impl NusbDevice {
    /// The worker of `address`, opening the endpoint on first use.
    fn endpoint(&self, address: u8) -> Result<mpsc::UnboundedSender<Job>, UsbError> {
        let mut st = self.state.lock().unwrap();
        if let Some((_, tx)) = st.endpoints.get(&address)
            && !tx.is_closed()
        {
            return Ok(tx.clone());
        }
        let mut found = None;
        for (num, intf) in &st.interfaces {
            if let Some(desc) = intf.descriptor()
                && let Some(ep) = desc.endpoints().find(|e| e.address() == address)
            {
                found = Some((*num, intf.clone(), ep.transfer_type()));
                break;
            }
        }
        let Some((num, intf, kind)) = found else {
            return Err(UsbError::NotClaimed(format!(
                "endpoint 0x{address:02x} is not on a claimed interface"
            )));
        };
        let is_in = address & 0x80 != 0;
        let open_err = |e: nusb::Error| UsbError::Failed(format!("open endpoint 0x{address:02x}: {e}"));
        let tx = match (kind, is_in) {
            (TransferType::Bulk, true) => spawn_worker(intf.endpoint::<Bulk, In>(address).map_err(open_err)?, true),
            (TransferType::Bulk, false) => spawn_worker(intf.endpoint::<Bulk, Out>(address).map_err(open_err)?, false),
            (TransferType::Interrupt, true) => {
                spawn_worker(intf.endpoint::<Interrupt, In>(address).map_err(open_err)?, true)
            }
            (TransferType::Interrupt, false) => {
                spawn_worker(intf.endpoint::<Interrupt, Out>(address).map_err(open_err)?, false)
            }
            (t, _) => {
                return Err(UsbError::Failed(format!(
                    "endpoint 0x{address:02x} is {t:?}: not supported yet"
                )));
            }
        };
        st.endpoints.insert(address, (num, tx.clone()));
        Ok(tx)
    }

    fn max_packet(&self, address: u8) -> usize {
        let st = self.state.lock().unwrap();
        st.interfaces
            .values()
            .filter_map(|i| i.descriptor())
            .flat_map(|d| d.endpoints().collect::<Vec<_>>())
            .find(|e| e.address() == address)
            .map(|e| e.max_packet_size())
            .unwrap_or(512)
            .max(1)
    }
}

fn split_request_type(t: u8) -> (ControlType, Recipient) {
    let ct = match (t >> 5) & 3 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        _ => ControlType::Vendor,
    };
    let r = match t & 0x1f {
        0 => Recipient::Device,
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        _ => Recipient::Other,
    };
    (ct, r)
}

impl UsbDevice for NusbDevice {
    fn claim(&self, interface: u8, detach: bool) -> BoxFut<Result<(), UsbError>> {
        if self.state.lock().unwrap().interfaces.contains_key(&interface) {
            return Box::pin(async { Ok(()) });
        }
        let device = self.device.clone();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let res = if detach {
                device.detach_and_claim_interface(interface).await
            } else {
                device.claim_interface(interface).await
            };
            match res {
                Ok(intf) => {
                    state.lock().unwrap().interfaces.insert(interface, intf);
                    Ok(())
                }
                Err(e) => Err(UsbError::Failed(format!("claim interface {interface}: {e}"))),
            }
        })
    }

    fn release(&self, interface: u8) -> BoxFut<Result<(), UsbError>> {
        let mut st = self.state.lock().unwrap();
        st.endpoints.retain(|_, (owner, _)| *owner != interface);
        st.interfaces.remove(&interface);
        Box::pin(async { Ok(()) })
    }

    fn set_alternate(&self, interface: u8, alternate: u8) -> BoxFut<Result<(), UsbError>> {
        let mut st = self.state.lock().unwrap();
        // Endpoints change with the alternate setting: reopen them on next use.
        st.endpoints.retain(|_, (owner, _)| *owner != interface);
        let intf = st.interfaces.get(&interface).cloned();
        drop(st);
        Box::pin(async move {
            let Some(intf) = intf else {
                return Err(UsbError::NotClaimed(format!("interface {interface} is not claimed")));
            };
            intf.set_alt_setting(alternate)
                .await
                .map_err(|e| UsbError::Failed(format!("set alternate: {e}")))
        })
    }

    fn control(&self, setup: Setup, data: Vec<u8>, timeout: Duration) -> BoxFut<TransferResult> {
        let device = self.device.clone();
        let (control_type, recipient) = split_request_type(setup.request_type);
        Box::pin(async move {
            if setup.is_in() {
                let req = ControlIn {
                    control_type,
                    recipient,
                    request: setup.request,
                    value: setup.value,
                    index: setup.index,
                    length: setup.length,
                };
                match device.control_in(req, timeout).await {
                    Ok(v) => TransferResult::ok_in(v),
                    Err(e) => TransferResult::failed(status(e)),
                }
            } else {
                let req = ControlOut {
                    control_type,
                    recipient,
                    request: setup.request,
                    value: setup.value,
                    index: setup.index,
                    data: &data,
                };
                match device.control_out(req, timeout).await {
                    Ok(()) => TransferResult::ok_out(data.len()),
                    Err(e) => TransferResult::failed(status(e)),
                }
            }
        })
    }

    fn transfer(&self, endpoint: u8, length: u32, data: Vec<u8>, timeout: Duration) -> BoxFut<TransferResult> {
        let tx = match self.endpoint(endpoint) {
            Ok(tx) => tx,
            Err(e) => {
                tracing::debug!("transfer on 0x{endpoint:02x}: {e}");
                return Box::pin(async { TransferResult::failed(UsbStatus::InvalidArgument) });
            }
        };
        let is_in = endpoint & 0x80 != 0;
        let (buf, want) = if is_in {
            // usbfs needs IN lengths in whole packets; extra bytes are dropped.
            let mps = self.max_packet(endpoint);
            let want = length as usize;
            (Buffer::new(want.div_ceil(mps).max(1) * mps), want)
        } else {
            let n = data.len();
            (Buffer::from(data), n)
        };
        let (reply, rx) = oneshot::channel();
        let queued = tx
            .send(Job::Transfer {
                buf,
                want,
                timeout,
                reply,
            })
            .is_ok();
        Box::pin(async move {
            if !queued {
                return TransferResult::failed(UsbStatus::Disconnected);
            }
            rx.await
                .unwrap_or_else(|_| TransferResult::failed(UsbStatus::Cancelled))
        })
    }

    fn clear_halt(&self, endpoint: u8) -> BoxFut<Result<(), UsbError>> {
        let tx = self.endpoint(endpoint);
        Box::pin(async move {
            let tx = tx?;
            let (reply, rx) = oneshot::channel();
            tx.send(Job::ClearHalt { reply })
                .map_err(|_| UsbError::Failed("endpoint closed".into()))?;
            rx.await
                .unwrap_or_else(|_| Err(UsbError::Failed("endpoint closed".into())))
        })
    }

    fn reset(&self) -> BoxFut<Result<(), UsbError>> {
        let device = self.device.clone();
        {
            let mut st = self.state.lock().unwrap();
            st.endpoints.clear();
            st.interfaces.clear();
        }
        Box::pin(async move {
            device
                .reset()
                .await
                .map_err(|e| UsbError::Failed(format!("reset: {e}")))
        })
    }

    fn close(&self) {
        {
            let mut st = self.state.lock().unwrap();
            st.endpoints.clear();
            st.interfaces.clear();
        }
        (self.closer)(self.id);
    }
}
