//! USB access behind the agent: what a platform backend must offer.
//!
//! Android: [`crate::nusb_backend`] on the usbfs file descriptor of an opened
//! `UsbDeviceConnection`. Simulator and tests: [`crate::fake`].
//!
//! Ordering matters: transfers on one endpoint must reach the device in the order the host
//! sent them. So [`UsbDevice::transfer`] and [`UsbDevice::control`] *queue* the request when
//! called (synchronously, in the session loop) and the returned future only waits for the
//! result.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hubbridge_proto::{Setup, UsbStatus};

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbError {
    /// The user has not granted USB permission (Android dialog).
    NoPermission(String),
    NotFound,
    /// The endpoint is not on a claimed interface, and similar misuse.
    NotClaimed(String),
    Failed(String),
}

impl std::fmt::Display for UsbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsbError::NoPermission(m) => write!(f, "no USB permission: {m}"),
            UsbError::NotFound => f.write_str("device not found"),
            UsbError::NotClaimed(m) => write!(f, "{m}"),
            UsbError::Failed(m) => write!(f, "{m}"),
        }
    }
}

/// Result of a control / bulk / interrupt transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferResult {
    pub status: UsbStatus,
    /// IN: the bytes received. OUT: empty.
    pub data: Vec<u8>,
    /// Bytes actually transferred (IN: `data.len()`; OUT: bytes sent).
    pub actual: u32,
}

impl TransferResult {
    pub fn ok_in(data: Vec<u8>) -> Self {
        let actual = data.len() as u32;
        TransferResult {
            status: UsbStatus::Ok,
            data,
            actual,
        }
    }

    pub fn ok_out(sent: usize) -> Self {
        TransferResult {
            status: UsbStatus::Ok,
            data: Vec::new(),
            actual: sent as u32,
        }
    }

    pub fn failed(status: UsbStatus) -> Self {
        TransferResult {
            status,
            data: Vec::new(),
            actual: 0,
        }
    }
}

/// Opens devices by agent device id.
pub trait UsbBackend: Send + Sync {
    fn open(&self, id: u32) -> BoxFut<Result<Arc<dyn UsbDevice>, UsbError>>;
}

/// An opened device. All methods may be called concurrently.
pub trait UsbDevice: Send + Sync {
    fn claim(&self, interface: u8, detach_kernel_driver: bool) -> BoxFut<Result<(), UsbError>>;
    fn release(&self, interface: u8) -> BoxFut<Result<(), UsbError>>;
    fn set_alternate(&self, interface: u8, alternate: u8) -> BoxFut<Result<(), UsbError>>;
    /// Queues a control transfer now; the future waits for the result.
    fn control(&self, setup: Setup, data: Vec<u8>, timeout: Duration) -> BoxFut<TransferResult>;
    /// Queues a bulk/interrupt transfer now; the future waits for the result.
    /// IN (`endpoint & 0x80`): read up to `length` bytes. OUT: send `data`.
    fn transfer(&self, endpoint: u8, length: u32, data: Vec<u8>, timeout: Duration) -> BoxFut<TransferResult>;
    fn clear_halt(&self, endpoint: u8) -> BoxFut<Result<(), UsbError>>;
    fn reset(&self) -> BoxFut<Result<(), UsbError>>;
    /// Release interfaces, cancel transfers, give the device back to the OS.
    fn close(&self);
}

/// Upper bound for one transfer, to keep memory bounded.
pub const MAX_TRANSFER: u32 = 1024 * 1024;
/// Upper bound for a transfer timeout from the host.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);

pub fn timeout_from_ms(ms: u32) -> Duration {
    if ms == 0 {
        MAX_TIMEOUT
    } else {
        Duration::from_millis(ms as u64).min(MAX_TIMEOUT)
    }
}
