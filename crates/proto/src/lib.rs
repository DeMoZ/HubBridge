//! HubBridge wire protocol.
//!
//! This crate has no I/O and no crypto. It defines:
//! - the plaintext hello exchanged before the encrypted session ([`ClientHello`], [`ServerHello`]);
//! - the application messages carried inside the encrypted session ([`Frame`], [`Message`]);
//! - the USB device model shared by the agent and the host ([`usb`]);
//! - a parser for raw USB descriptors as Android's `UsbDeviceConnection.getRawDescriptors()`
//!   returns them ([`descriptors`]).
//!
//! Everything is encoded with `postcard` (compact, serde-based). See `docs/PROTOCOL.md`.

pub mod descriptors;
pub mod fixtures;
pub mod usb;

use serde::{Deserialize, Serialize};

pub use usb::{ConfigInfo, EndpointInfo, InterfaceInfo, TransferType, UsbDeviceInfo};

/// Protocol version. Bumped on any incompatible change of the hello or the message set.
pub const PROTO_VERSION: u16 = 1;

/// First bytes of every [`ClientHello`], to reject non-HubBridge peers early.
pub const MAGIC: [u8; 4] = *b"HBRG";

/// Default TCP port the agent listens on (Wi-Fi/LAN and the device side of `adb forward`).
pub const DEFAULT_PORT: u16 = 47800;

/// DNS-SD service type the agent advertises (Android NSD / mDNS).
pub const MDNS_SERVICE_TYPE: &str = "_hubbridge._tcp.local.";
/// Same type in the form Android `NsdManager` expects.
pub const NSD_SERVICE_TYPE: &str = "_hubbridge._tcp";

/// Largest application message (after reassembly) either side accepts.
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// What the client wants from this connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Intent {
    /// First contact: pair with the PIN shown on the phone. Carries the SPAKE2 message.
    Pair { spake_msg: Vec<u8> },
    /// Already paired: open a session with the stored key.
    Resume,
}

/// Sent by the host right after the TCP connection opens (plaintext).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub magic: [u8; 4],
    pub version: u16,
    /// Stable random id of this host installation (hex).
    pub host_id: String,
    /// Human-readable host name, shown on the phone.
    pub host_name: String,
    pub intent: Intent,
}

/// Agent's answer to [`ClientHello`] (plaintext).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub version: u16,
    pub agent_id: String,
    pub agent_name: String,
    pub result: HelloResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloResult {
    /// Go on with the Noise handshake. For pairing it carries the agent's SPAKE2 message.
    Proceed {
        spake_msg: Option<Vec<u8>>,
    },
    Rejected {
        code: HelloError,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloError {
    BadMagic,
    VersionMismatch,
    /// Resume from a host the agent does not know: pair first.
    NotPaired,
    /// Pair requested but the phone is not showing a PIN right now.
    PairingNotActive,
    /// Too many wrong PINs: a new PIN must be started on the phone.
    PairingLocked,
    Busy,
}

/// One application message inside the encrypted session.
///
/// `req` correlates a response with its request: the side that sends a request picks a
/// non-zero id, the response carries the same id. Unsolicited events use `req = 0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub req: u32,
    pub msg: Message,
}

/// Application messages. New variants are only appended at the end (postcard encodes the
/// variant index), so an older peer fails cleanly with a decode error it can report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// Liveness and RTT probe. `sent_us` is opaque to the receiver and echoed back.
    Ping {
        seq: u32,
        sent_us: u64,
    },
    Pong {
        seq: u32,
        sent_us: u64,
    },

    GetAgentInfo,
    AgentInfo(AgentInfo),

    ListDevices,
    DeviceList {
        devices: Vec<UsbDeviceInfo>,
    },
    /// Event: a device appeared or changed (e.g. permission granted, descriptors now known).
    DeviceAdded(UsbDeviceInfo),
    /// Event: a device was unplugged.
    DeviceRemoved {
        id: u32,
    },

    /// Ask the agent to show Android's USB permission dialog for this device.
    RequestPermission {
        id: u32,
    },
    /// Response to [`Message::RequestPermission`]: the dialog was shown (the outcome arrives
    /// later as a [`Message::DeviceAdded`] event with `has_permission = true`).
    PermissionRequested {
        id: u32,
    },

    Error {
        code: ErrorCode,
        message: String,
    },
    /// Orderly close.
    Bye,

    // --- Step 2: USB data (host → agent requests; `Done` / `*Done` responses) -----------
    /// Open the device for this session (exclusive across sessions). Needs USB permission.
    OpenDevice {
        id: u32,
    },
    CloseDevice {
        id: u32,
    },
    /// Claim an interface; `detach_kernel_driver` takes it from the phone's own driver
    /// (e.g. Android unmounts a flash drive, stops using a keyboard).
    ClaimInterface {
        id: u32,
        interface: u8,
        detach_kernel_driver: bool,
    },
    ReleaseInterface {
        id: u32,
        interface: u8,
    },
    SetAlternate {
        id: u32,
        interface: u8,
        alternate: u8,
    },
    /// Control transfer on endpoint 0. Direction from `setup.request_type` bit 7:
    /// IN reads `setup.length` bytes, OUT sends `data`.
    Control {
        id: u32,
        setup: Setup,
        data: Vec<u8>,
        timeout_ms: u32,
    },
    /// Bulk or interrupt transfer. IN (`endpoint` bit 7 set) reads up to `length` bytes;
    /// OUT sends `data`. Several may be in flight per endpoint; they complete in order.
    Transfer {
        id: u32,
        endpoint: u8,
        length: u32,
        data: Vec<u8>,
        timeout_ms: u32,
    },
    ClearHalt {
        id: u32,
        endpoint: u8,
    },
    ResetDevice {
        id: u32,
    },
    /// Success of a request that returns nothing.
    Done,
    ControlDone {
        status: UsbStatus,
        data: Vec<u8>,
    },
    TransferDone {
        status: UsbStatus,
        data: Vec<u8>,
        actual: u32,
    },
    /// Event: an open device was closed by the agent (unplugged, permission revoked, error).
    DeviceClosed {
        id: u32,
        reason: String,
    },
}

/// SETUP packet of a control transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    /// `bmRequestType`: bit 7 direction (1 = IN), bits 5..6 type, bits 0..4 recipient.
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    pub fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }

    /// Standard GET_DESCRIPTOR to the device.
    pub fn get_descriptor(kind: u8, index: u8, lang: u16, length: u16) -> Self {
        Setup {
            request_type: 0x80,
            request: 0x06,
            value: (kind as u16) << 8 | index as u16,
            index: lang,
            length,
        }
    }
}

/// Outcome of a USB transfer as the device / OS reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbStatus {
    Ok,
    /// The endpoint stalled (device error); clear it with `ClearHalt`.
    Stall,
    /// Timed out or cancelled.
    Cancelled,
    Disconnected,
    /// Hardware or protocol error.
    Fault,
    /// The OS rejected the request (wrong length, type, endpoint).
    InvalidArgument,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    UnknownDevice,
    Unsupported,
    Internal,
    /// The user has not granted USB permission for this device on the phone.
    NoPermission,
    /// Another session has the device open.
    Busy,
    /// The device is not open in this session (or the interface is not claimed).
    NotOpen,
    /// The OS refused a USB operation (claim, set alternate, reset...).
    UsbFailed,
}

/// Static facts about the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_id: String,
    pub name: String,
    /// e.g. "Pixel 8 Pro" or "simulator".
    pub model: String,
    /// e.g. "Android 15 (API 35)".
    pub os: String,
    pub app_version: String,
    pub proto_version: u16,
    /// Whether the phone reports USB host support (`android.hardware.usb.host`).
    pub usb_host: bool,
}

/// Encode a value with postcard.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(value)
}

/// Decode a value with postcard.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let frame = Frame {
            req: 7,
            msg: Message::DeviceList {
                devices: vec![fixtures::cp2102_uart(1), fixtures::hid_keyboard(2)],
            },
        };
        let bytes = encode(&frame).unwrap();
        let back: Frame = decode(&bytes).unwrap();
        assert_eq!(frame, back);
    }

    #[test]
    fn hello_roundtrip() {
        let hello = ClientHello {
            magic: MAGIC,
            version: PROTO_VERSION,
            host_id: "ab".into(),
            host_name: "mac".into(),
            intent: Intent::Pair {
                spake_msg: vec![1, 2, 3],
            },
        };
        let back: ClientHello = decode(&encode(&hello).unwrap()).unwrap();
        assert_eq!(hello, back);
    }
}
