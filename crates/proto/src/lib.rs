//! HubBridge wire protocol.
//!
//! This crate has no I/O and no crypto. It defines:
//! - the plaintext hello exchanged before the encrypted session ([`ClientHello`], [`ServerHello`]);
//! - the application messages carried inside the encrypted session ([`Frame`], [`Message`]);
//! - the USB device model shared by the agent and the host ([`usb`]);
//! - a parser for raw USB descriptors as Android's `UsbDeviceConnection.getRawDescriptors()`
//!   and Linux usbfs return them ([`descriptors`]).
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    UnknownDevice,
    Unsupported,
    Internal,
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
