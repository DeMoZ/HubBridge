//! HubBridge secure link.
//!
//! Works over any ordered byte stream (TCP over Wi-Fi/LAN, TCP through `adb forward`).
//!
//! Connection setup:
//! 1. Host → agent: [`ClientHello`] (plaintext record). For pairing it carries a SPAKE2
//!    message built from the 6-digit PIN shown on the phone.
//! 2. Agent → host: [`ServerHello`] (plaintext record), with the agent's SPAKE2 message
//!    when pairing.
//! 3. Noise `NNpsk0_25519_ChaChaPoly_BLAKE2s` handshake (two records). The PSK is derived
//!    from the SPAKE2 secret (pairing) or from the long-term key stored at pairing time
//!    (resume). Both hellos are bound into the Noise prologue, so tampering with them
//!    breaks the handshake. A wrong PIN or key fails here.
//! 4. Encrypted records carrying postcard [`Frame`]s.
//!
//! Why SPAKE2 and not "HMAC of the PIN": a 6-digit PIN has only 10^6 values. With a PAKE an
//! eavesdropper learns nothing it can brute-force offline, and an active attacker gets one
//! guess per connection; the agent limits attempts.
//!
//! Record layer: `u16` big-endian length + body. In the session each body is one Noise
//! message (≤ 65535 bytes). An application message is a `u32` big-endian length followed by
//! the postcard bytes, split over as many records as needed.
//!
//! The sender is cancel-safe: all records of a message are encrypted into a buffer
//! first, and a send that is dropped part way leaves the unwritten rest in that buffer, to
//! be finished by the next `send` / `send_bytes` / `shutdown`. The nonce sequence and the
//! byte stream the peer sees are therefore never corrupted by a dropped future.

use std::io;
use std::sync::Arc;

use hkdf::Hkdf;
use hubbridge_proto::{
    self as proto, ClientHello, Frame, HelloError, HelloResult, Intent, MAGIC, MAX_MESSAGE_LEN, PROTO_VERSION,
    ServerHello,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use snow::{HandshakeState, StatelessTransportState};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const NOISE_MAX: usize = 65535;
const TAG_LEN: usize = 16;
const MAX_PLAIN: usize = NOISE_MAX - TAG_LEN;
const SPAKE_ID_HOST: &[u8] = b"hubbridge-host";
const SPAKE_ID_AGENT: &[u8] = b"hubbridge-agent";
/// Largest plaintext hello record we accept.
const MAX_HELLO: usize = 4096;

#[derive(Debug, Error)]
pub enum LinkError {
    #[error("connection closed")]
    Closed,
    #[error("i/o: {0}")]
    Io(io::Error),
    #[error("decode: {0}")]
    Decode(#[from] postcard::Error),
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("not a HubBridge peer")]
    BadMagic,
    #[error("protocol version mismatch: ours {ours}, theirs {theirs}")]
    Version { ours: u16, theirs: u16 },
    #[error("agent rejected the connection ({code:?}): {message}")]
    Rejected { code: HelloError, message: String },
    #[error("pairing failed: wrong PIN, or the PIN expired")]
    PairingFailed,
    #[error("authentication failed: the agent does not accept the stored key (pair again)")]
    AuthFailed,
    #[error("this host is not paired with agent {0}: pair first")]
    UnknownAgent(String),
    #[error("message too large: {0} bytes")]
    TooLarge(usize),
    #[error("protocol error: {0}")]
    Protocol(&'static str),
}

impl From<io::Error> for LinkError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe => LinkError::Closed,
            _ => LinkError::Io(e),
        }
    }
}

/// A 32-byte secret. Its `Debug` never shows the value; on disk it is hex.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub [u8; 32]);

impl Secret {
    pub fn random() -> Self {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).expect("OS random generator");
        Secret(k)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("key must be 32 bytes"))?;
        Ok(Secret(arr))
    }
}

/// Random id (16 bytes, hex) for hosts and agents.
pub fn random_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS random generator");
    hex::encode(b)
}

/// Random 6-digit PIN.
pub fn random_pin() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("OS random generator");
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}

// ---------------------------------------------------------------------------------------
// Record layer
// ---------------------------------------------------------------------------------------

async fn write_record<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> Result<(), LinkError> {
    let len: u16 = body.len().try_into().map_err(|_| LinkError::TooLarge(body.len()))?;
    let mut out = Vec::with_capacity(2 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    w.write_all(&out).await?;
    w.flush().await?;
    Ok(())
}

async fn read_record<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> Result<Vec<u8>, LinkError> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let len = u16::from_be_bytes(len) as usize;
    if len > max {
        return Err(LinkError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
}

// ---------------------------------------------------------------------------------------
// Encrypted session halves
// ---------------------------------------------------------------------------------------

/// Sending half of an established session.
pub struct SecureSender<W> {
    w: W,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
    /// Encrypted records not yet fully written (left over from a dropped send).
    out: Vec<u8>,
    /// Bytes of `out` already written.
    done: usize,
}

/// Receiving half of an established session.
pub struct SecureReceiver<R> {
    r: R,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
}

impl<W: AsyncWrite + Unpin> SecureSender<W> {
    /// Encode and send one frame. Cancel-safe, see [`SecureSender::send_bytes`].
    pub async fn send(&mut self, frame: &Frame) -> Result<(), LinkError> {
        let bytes = proto::encode(frame)?;
        self.send_bytes(&bytes).await
    }

    /// Send one message (split into records as needed).
    ///
    /// Cancel-safe: if the future is dropped, the message is either not started or will be
    /// finished by the next `send` / `send_bytes` / `shutdown`; the record stream is never
    /// corrupted. A dropped send may therefore still be delivered later. After an I/O error
    /// the sender must not be used again.
    pub async fn send_bytes(&mut self, data: &[u8]) -> Result<(), LinkError> {
        // Finish what an earlier, dropped send left behind, before anything new.
        self.drain().await?;
        if data.len() > MAX_MESSAGE_LEN {
            return Err(LinkError::TooLarge(data.len()));
        }
        let mut plain = Vec::with_capacity(4 + data.len());
        plain.extend_from_slice(&(data.len() as u32).to_be_bytes());
        plain.extend_from_slice(data);

        // Encrypt everything first: an encryption error leaves `out` and the nonce untouched.
        let chunks = plain.len().div_ceil(MAX_PLAIN);
        let mut enc = Vec::with_capacity(plain.len() + chunks * (2 + TAG_LEN));
        let mut nonce = self.nonce;
        let mut ct = vec![0u8; NOISE_MAX];
        for chunk in plain.chunks(MAX_PLAIN) {
            let n = self.transport.write_message(nonce, chunk, &mut ct)?;
            nonce += 1;
            enc.extend_from_slice(&(n as u16).to_be_bytes());
            enc.extend_from_slice(&ct[..n]);
        }
        // From here the bytes are committed: the nonces are taken and the records sit in
        // `out`, which is written before anything else.
        self.nonce = nonce;
        self.out = enc;
        self.done = 0;
        self.drain().await
    }

    /// Write `out[done..]`, flush, and clear the buffer. Dropping it part way is fine:
    /// `done` only advances after a successful `write`, and a repeated flush is harmless.
    async fn drain(&mut self) -> Result<(), LinkError> {
        if self.out.is_empty() {
            return Ok(());
        }
        while self.done < self.out.len() {
            let n = self.w.write(&self.out[self.done..]).await?;
            if n == 0 {
                return Err(LinkError::Io(io::ErrorKind::WriteZero.into()));
            }
            self.done += n;
        }
        self.w.flush().await?;
        // Release the buffer: a large message's capacity is not kept for the session.
        self.out = Vec::new();
        self.done = 0;
        Ok(())
    }

    /// Best-effort orderly shutdown of the write side: first finishes the bytes a dropped
    /// send left behind, flushes, then shuts the write side down. Cancel-safe (a dropped
    /// shutdown can be repeated).
    pub async fn shutdown(&mut self) {
        let _ = self.drain().await;
        let _ = self.w.flush().await;
        let _ = self.w.shutdown().await;
    }
}

impl<R: AsyncRead + Unpin> SecureReceiver<R> {
    pub async fn recv(&mut self) -> Result<Frame, LinkError> {
        let bytes = self.recv_bytes().await?;
        Ok(proto::decode(&bytes)?)
    }

    pub async fn recv_bytes(&mut self) -> Result<Vec<u8>, LinkError> {
        let mut first = self.read_plain().await?;
        if first.len() < 4 {
            return Err(LinkError::Protocol("short message header"));
        }
        let total = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
        if total > MAX_MESSAGE_LEN {
            return Err(LinkError::TooLarge(total));
        }
        first.drain(..4);
        let mut out = first;
        while out.len() < total {
            let more = self.read_plain().await?;
            out.extend_from_slice(&more);
        }
        if out.len() != total {
            return Err(LinkError::Protocol("message length mismatch"));
        }
        Ok(out)
    }

    async fn read_plain(&mut self) -> Result<Vec<u8>, LinkError> {
        let ct = read_record(&mut self.r, NOISE_MAX).await?;
        let mut plain = vec![0u8; ct.len()];
        let n = self.transport.read_message(self.nonce, &ct, &mut plain)?;
        self.nonce += 1;
        plain.truncate(n);
        Ok(plain)
    }
}

// ---------------------------------------------------------------------------------------
// Key derivation
// ---------------------------------------------------------------------------------------

fn transcript(client_hello: &[u8], server_hello: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"hubbridge transcript v1");
    h.update((client_hello.len() as u32).to_be_bytes());
    h.update(client_hello);
    h.update((server_hello.len() as u32).to_be_bytes());
    h.update(server_hello);
    h.finalize().into()
}

fn derive(ikm: &[u8], salt: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out).expect("32 bytes is a valid HKDF length");
    out
}

/// PSK for the Noise handshake and, for pairing, the new long-term key.
fn session_keys(secret: &[u8], pairing: bool, transcript: &[u8; 32]) -> ([u8; 32], Option<Secret>) {
    if pairing {
        let psk = derive(secret, transcript, b"hubbridge pairing psk");
        let ltk = derive(secret, transcript, b"hubbridge long-term key");
        (psk, Some(Secret(ltk)))
    } else {
        (derive(secret, transcript, b"hubbridge resume psk"), None)
    }
}

fn noise(psk: &[u8; 32], prologue: &[u8; 32], initiator: bool) -> Result<HandshakeState, LinkError> {
    let params = NOISE_PARAMS.parse().expect("valid noise params");
    let b = snow::Builder::new(params).psk(0, psk)?.prologue(prologue)?;
    Ok(if initiator {
        b.build_initiator()?
    } else {
        b.build_responder()?
    })
}

fn split<R, W>(r: R, w: W, hs: HandshakeState) -> Result<(SecureSender<W>, SecureReceiver<R>), LinkError> {
    let t = Arc::new(hs.into_stateless_transport_mode()?);
    Ok((
        SecureSender {
            w,
            transport: t.clone(),
            nonce: 0,
            out: Vec::new(),
            done: 0,
        },
        SecureReceiver {
            r,
            transport: t,
            nonce: 0,
        },
    ))
}

// ---------------------------------------------------------------------------------------
// Host (client) side
// ---------------------------------------------------------------------------------------

/// Looks up the stored key for an agent id (the agent names itself in its hello).
pub type KeyLookup = Box<dyn FnOnce(&str) -> Option<Secret> + Send>;

/// How the host authenticates.
pub enum ClientAuth {
    /// First contact: the PIN shown on the phone.
    Pair { pin: String },
    /// The long-term key saved when pairing with this agent, chosen once the agent
    /// sent its id, so the host does not need to know in advance which phone answers.
    Resume { key_for: KeyLookup },
}

impl ClientAuth {
    /// Resume with one known key.
    pub fn resume_with(key: Secret) -> Self {
        ClientAuth::Resume {
            key_for: Box::new(move |_| Some(key)),
        }
    }
}

/// Result of a successful host-side handshake.
pub struct ClientSession<R, W> {
    pub sender: SecureSender<W>,
    pub receiver: SecureReceiver<R>,
    pub agent_id: String,
    pub agent_name: String,
    /// Set after pairing: save it, it is the key for every later [`ClientAuth::Resume`].
    pub new_key: Option<Secret>,
}

pub async fn client_handshake<R, W>(
    mut r: R,
    mut w: W,
    host_id: &str,
    host_name: &str,
    auth: ClientAuth,
) -> Result<ClientSession<R, W>, LinkError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (spake, intent) = match &auth {
        ClientAuth::Pair { pin } => {
            let (s, msg) = Spake2::<Ed25519Group>::start_a(
                &Password::new(pin.trim().as_bytes()),
                &Identity::new(SPAKE_ID_HOST),
                &Identity::new(SPAKE_ID_AGENT),
            );
            (Some(s), Intent::Pair { spake_msg: msg })
        }
        ClientAuth::Resume { .. } => (None, Intent::Resume),
    };
    let hello = ClientHello {
        magic: MAGIC,
        version: PROTO_VERSION,
        host_id: host_id.to_string(),
        host_name: host_name.to_string(),
        intent,
    };
    let hello_bytes = proto::encode(&hello)?;
    write_record(&mut w, &hello_bytes).await?;

    let server_bytes = read_record(&mut r, MAX_HELLO).await?;
    let server: ServerHello = proto::decode(&server_bytes)?;
    if server.version != PROTO_VERSION {
        return Err(LinkError::Version {
            ours: PROTO_VERSION,
            theirs: server.version,
        });
    }
    let their_spake = match server.result {
        HelloResult::Rejected { code, message } => return Err(LinkError::Rejected { code, message }),
        HelloResult::Proceed { spake_msg } => spake_msg,
    };

    let tr = transcript(&hello_bytes, &server_bytes);
    let pairing = spake.is_some();
    let secret: Vec<u8> = match (spake, auth) {
        (Some(s), _) => {
            let msg = their_spake.ok_or(LinkError::Protocol("agent sent no SPAKE2 message"))?;
            s.finish(&msg).map_err(|_| LinkError::Protocol("bad SPAKE2 message"))?
        }
        (None, ClientAuth::Resume { key_for }) => match key_for(&server.agent_id) {
            Some(key) => key.0.to_vec(),
            None => return Err(LinkError::UnknownAgent(server.agent_id)),
        },
        (None, ClientAuth::Pair { .. }) => unreachable!(),
    };
    let (psk, new_key) = session_keys(&secret, pairing, &tr);

    let mut hs = noise(&psk, &tr, true)?;
    let mut buf = vec![0u8; NOISE_MAX];
    let n = hs.write_message(&[], &mut buf)?;
    write_record(&mut w, &buf[..n]).await?;

    // The agent closes the connection if it cannot decrypt our first message.
    let fail = if pairing {
        LinkError::PairingFailed
    } else {
        LinkError::AuthFailed
    };
    let reply = match read_record(&mut r, NOISE_MAX).await {
        Ok(reply) => reply,
        Err(LinkError::Closed) => return Err(fail),
        Err(e) => return Err(e),
    };
    let mut payload = vec![0u8; NOISE_MAX];
    hs.read_message(&reply, &mut payload).map_err(|_| fail)?;

    let (sender, receiver) = split(r, w, hs)?;
    Ok(ClientSession {
        sender,
        receiver,
        agent_id: server.agent_id,
        agent_name: server.agent_name,
        new_key,
    })
}

// ---------------------------------------------------------------------------------------
// Agent (server) side
// ---------------------------------------------------------------------------------------

/// A hello the agent received, plus its exact bytes for the transcript.
pub struct ReceivedHello {
    pub hello: ClientHello,
    bytes: Vec<u8>,
}

/// What the agent decided after looking at the hello.
pub enum ServerDecision {
    /// Pair using this PIN (the one shown on screen).
    Pair {
        pin: String,
    },
    /// Resume with the stored key for this host.
    Resume {
        key: Secret,
    },
    Reject {
        code: HelloError,
        message: String,
    },
}

/// Result of a successful agent-side handshake.
pub struct ServerSession<R, W> {
    pub sender: SecureSender<W>,
    pub receiver: SecureReceiver<R>,
    /// Set after pairing: store it for `hello.host_id`.
    pub new_key: Option<Secret>,
}

/// Read and validate the [`ClientHello`]. Version and magic errors are answered here.
pub async fn server_read_hello<R, W>(
    r: &mut R,
    w: &mut W,
    agent_id: &str,
    agent_name: &str,
) -> Result<ReceivedHello, LinkError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let bytes = read_record(r, MAX_HELLO).await?;
    let hello: ClientHello = match proto::decode(&bytes) {
        Ok(h) => h,
        Err(_) => return Err(LinkError::BadMagic),
    };
    let reject = |code, message: &str| ServerHello {
        version: PROTO_VERSION,
        agent_id: agent_id.to_string(),
        agent_name: agent_name.to_string(),
        result: HelloResult::Rejected {
            code,
            message: message.to_string(),
        },
    };
    if hello.magic != MAGIC {
        return Err(LinkError::BadMagic);
    }
    if hello.version != PROTO_VERSION {
        let msg = format!("agent speaks protocol v{PROTO_VERSION}");
        write_record(w, &proto::encode(&reject(HelloError::VersionMismatch, &msg))?).await?;
        return Err(LinkError::Version {
            ours: PROTO_VERSION,
            theirs: hello.version,
        });
    }
    Ok(ReceivedHello { hello, bytes })
}

/// Answer the hello according to `decision` and run the Noise handshake.
pub async fn server_handshake<R, W>(
    mut r: R,
    mut w: W,
    received: &ReceivedHello,
    agent_id: &str,
    agent_name: &str,
    decision: ServerDecision,
) -> Result<ServerSession<R, W>, LinkError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut server = ServerHello {
        version: PROTO_VERSION,
        agent_id: agent_id.to_string(),
        agent_name: agent_name.to_string(),
        result: HelloResult::Proceed { spake_msg: None },
    };
    let (secret_source, pairing) = match decision {
        ServerDecision::Reject { code, message } => {
            server.result = HelloResult::Rejected {
                code,
                message: message.clone(),
            };
            write_record(&mut w, &proto::encode(&server)?).await?;
            return Err(LinkError::Rejected { code, message });
        }
        ServerDecision::Pair { pin } => {
            let Intent::Pair { spake_msg } = &received.hello.intent else {
                return Err(LinkError::Protocol("pair decision for a resume hello"));
            };
            let (s, msg) = Spake2::<Ed25519Group>::start_b(
                &Password::new(pin.trim().as_bytes()),
                &Identity::new(SPAKE_ID_HOST),
                &Identity::new(SPAKE_ID_AGENT),
            );
            server.result = HelloResult::Proceed { spake_msg: Some(msg) };
            let secret = s
                .finish(spake_msg)
                .map_err(|_| LinkError::Protocol("bad SPAKE2 message"))?;
            (secret, true)
        }
        ServerDecision::Resume { key } => (key.0.to_vec(), false),
    };
    let server_bytes = proto::encode(&server)?;
    write_record(&mut w, &server_bytes).await?;

    let tr = transcript(&received.bytes, &server_bytes);
    let (psk, new_key) = session_keys(&secret_source, pairing, &tr);
    let mut hs = noise(&psk, &tr, false)?;

    let first = read_record(&mut r, NOISE_MAX).await?;
    let mut payload = vec![0u8; NOISE_MAX];
    let fail = if pairing {
        LinkError::PairingFailed
    } else {
        LinkError::AuthFailed
    };
    hs.read_message(&first, &mut payload).map_err(|_| fail)?;
    let mut buf = vec![0u8; NOISE_MAX];
    let n = hs.write_message(&[], &mut buf)?;
    write_record(&mut w, &buf[..n]).await?;

    let (sender, receiver) = split(r, w, hs)?;
    Ok(ServerSession {
        sender,
        receiver,
        new_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hubbridge_proto::{Message, fixtures};
    use std::time::Duration;
    use tokio::io::{ReadHalf, WriteHalf, duplex, split as io_split};

    type Half = (ReadHalf<tokio::io::DuplexStream>, WriteHalf<tokio::io::DuplexStream>);

    fn pipe() -> (Half, Half) {
        let (a, b) = duplex(1 << 20);
        (io_split(a), io_split(b))
    }

    async fn agent(
        (mut r, mut w): Half,
        decide: impl FnOnce(&ClientHello) -> ServerDecision,
    ) -> Result<ServerSession<ReadHalf<tokio::io::DuplexStream>, WriteHalf<tokio::io::DuplexStream>>, LinkError> {
        let hello = server_read_hello(&mut r, &mut w, "agent1", "Phone").await?;
        let d = decide(&hello.hello);
        server_handshake(r, w, &hello, "agent1", "Phone", d).await
    }

    #[tokio::test]
    async fn pair_then_resume_and_exchange() {
        let (host_end, agent_end) = pipe();
        let agent_task = tokio::spawn(agent(agent_end, |_| ServerDecision::Pair { pin: "123456".into() }));
        let client = client_handshake(
            host_end.0,
            host_end.1,
            "host1",
            "Mac",
            ClientAuth::Pair { pin: "123456".into() },
        )
        .await
        .unwrap();
        let server = agent_task.await.unwrap().unwrap();
        let key = client.new_key.clone().unwrap();
        assert_eq!(Some(&key), server.new_key.as_ref());
        assert_eq!(client.agent_id, "agent1");

        // Resume with the stored key and send a large message (several records) each way.
        let (host_end, agent_end) = pipe();
        let k2 = key.clone();
        let agent_task = tokio::spawn(agent(agent_end, move |_| ServerDecision::Resume { key: k2 }));
        let mut client = client_handshake(host_end.0, host_end.1, "host1", "Mac", ClientAuth::resume_with(key))
            .await
            .unwrap();
        let mut server = agent_task.await.unwrap().unwrap();
        assert!(client.new_key.is_none());

        let big: Vec<_> = (0..300).map(fixtures::cp2102_uart).collect();
        let frame = Frame {
            req: 1,
            msg: Message::DeviceList { devices: big },
        };
        server.sender.send(&frame).await.unwrap();
        assert_eq!(client.receiver.recv().await.unwrap(), frame);
        let ping = Frame {
            req: 2,
            msg: Message::Ping { seq: 1, sent_us: 5 },
        };
        client.sender.send(&ping).await.unwrap();
        assert_eq!(server.receiver.recv().await.unwrap(), ping);
    }

    #[tokio::test]
    async fn wrong_pin_fails_on_both_sides() {
        let (host_end, agent_end) = pipe();
        let agent_task = tokio::spawn(async move {
            let res = agent(agent_end, |_| ServerDecision::Pair { pin: "111111".into() }).await;
            res.map(|_| ())
        });
        let res = client_handshake(
            host_end.0,
            host_end.1,
            "h",
            "Mac",
            ClientAuth::Pair { pin: "222222".into() },
        )
        .await;
        assert!(matches!(agent_task.await.unwrap(), Err(LinkError::PairingFailed)));
        assert!(matches!(res, Err(LinkError::PairingFailed)), "got {:?}", res.err());
    }

    #[tokio::test]
    async fn wrong_key_fails() {
        let (host_end, agent_end) = pipe();
        let agent_task = tokio::spawn(async move {
            agent(agent_end, |_| ServerDecision::Resume { key: Secret::random() })
                .await
                .map(|_| ())
        });
        let res = client_handshake(
            host_end.0,
            host_end.1,
            "h",
            "Mac",
            ClientAuth::resume_with(Secret::random()),
        )
        .await;
        assert!(matches!(agent_task.await.unwrap(), Err(LinkError::AuthFailed)));
        assert!(matches!(res, Err(LinkError::AuthFailed)), "got {:?}", res.err());
    }

    #[tokio::test]
    async fn rejection_is_reported() {
        let (host_end, agent_end) = pipe();
        let agent_task = tokio::spawn(async move {
            agent(agent_end, |_| ServerDecision::Reject {
                code: HelloError::NotPaired,
                message: "pair first".into(),
            })
            .await
            .map(|_| ())
        });
        let res = client_handshake(
            host_end.0,
            host_end.1,
            "h",
            "Mac",
            ClientAuth::resume_with(Secret::random()),
        )
        .await;
        let _ = agent_task.await;
        assert!(matches!(
            res,
            Err(LinkError::Rejected {
                code: HelloError::NotPaired,
                ..
            })
        ));
    }

    type Duplex = tokio::io::DuplexStream;

    /// A resumed session over a tiny pipe (`duplex(64)`).
    async fn tiny_session() -> (
        ClientSession<ReadHalf<Duplex>, WriteHalf<Duplex>>,
        ServerSession<ReadHalf<Duplex>, WriteHalf<Duplex>>,
    ) {
        let (a, b) = duplex(64);
        let (host_end, agent_end) = (io_split(a), io_split(b));
        let key = Secret([9; 32]);
        let k2 = key.clone();
        let agent_task = tokio::spawn(agent(agent_end, move |_| ServerDecision::Resume { key: k2 }));
        let client = client_handshake(host_end.0, host_end.1, "host1", "Mac", ClientAuth::resume_with(key))
            .await
            .unwrap();
        (client, agent_task.await.unwrap().unwrap())
    }

    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 + i / 251) as u8).collect()
    }

    /// Starts a 200 KiB send on a pipe nobody reads and drops it after a timeout.
    async fn drop_big_send(client: &mut ClientSession<ReadHalf<Duplex>, WriteHalf<Duplex>>) -> Vec<u8> {
        let big = patterned(200 * 1024);
        let res = tokio::time::timeout(Duration::from_millis(50), client.sender.send_bytes(&big)).await;
        assert!(res.is_err(), "the send must still be waiting on the full pipe");
        big
    }

    #[tokio::test]
    async fn send_dropped_mid_write_is_finished_by_next_send() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut client, mut server) = tiny_session().await;
            let big = drop_big_send(&mut client).await;
            let reader = tokio::spawn(async move {
                let first = server.receiver.recv_bytes().await.unwrap();
                let second = server.receiver.recv_bytes().await.unwrap();
                (first, second)
            });
            client.sender.send_bytes(b"second").await.unwrap();
            let (first, second) = reader.await.unwrap();
            assert!(first == big, "first message corrupted");
            assert_eq!(second, b"second");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn too_large_leaves_sender_usable() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut client, mut server) = tiny_session().await;
            let res = client.sender.send_bytes(&vec![0u8; MAX_MESSAGE_LEN + 1]).await;
            assert!(matches!(res, Err(LinkError::TooLarge(_))), "got {res:?}");
            let reader = tokio::spawn(async move { server.receiver.recv_bytes().await.unwrap() });
            client.sender.send_bytes(b"after").await.unwrap();
            assert_eq!(reader.await.unwrap(), b"after");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn shutdown_finishes_leftover() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut client, mut server) = tiny_session().await;
            let big = drop_big_send(&mut client).await;
            let reader = tokio::spawn(async move {
                let first = server.receiver.recv_bytes().await.unwrap();
                let end = server.receiver.recv_bytes().await;
                (first, end)
            });
            client.sender.shutdown().await;
            let (first, end) = reader.await.unwrap();
            assert!(first == big, "leftover message corrupted");
            assert!(matches!(end, Err(LinkError::Closed)), "got {end:?}");
        })
        .await
        .expect("test timed out");
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret([7; 32]);
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
    }
}
