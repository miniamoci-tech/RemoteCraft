//! RemoteCraft networking: a client over the relay, plus an end-to-end
//! encrypted, password-authenticated channel between two peers.
//!
//! Layers:
//! - [`RelayClient`] registers with a relay, is addressed by numeric ID, and
//!   moves opaque [`remotecraft_core`] `Data` frames to/from the paired peer.
//! - [`SecureChannel`] runs a Noise handshake over that transport. The shared
//!   secret is the session **password**, mixed in as a pre-shared key, so a
//!   peer that does not know the password cannot complete the handshake, and
//!   the relay (which only sees ciphertext) cannot read the session.
//!
//! The password is the whole access-control story:
//! - *attended* ("accesso vigilato"): the host shows a fresh random password
//!   via [`random_password`]; the person at the host reads it out.
//! - *unattended* ("accesso non vigilato"): the host owner sets a fixed
//!   password in advance.
//!
//! Both are the same mechanism; only the lifetime of the password differs.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use remotecraft_core::{
    checked_frame_len, decode_frame, encode_frame, FromRelay, PeerId, ToRelay, PROTOCOL_VERSION,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The Noise handshake pattern. `NNpsk0`: no static keys, ephemeral
/// Diffie-Hellman, with the password-derived pre-shared key mixed in first.
const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// Domain-separation prefix so the password KDF output can't be confused with
/// any other use of SHA-256 in the system.
const PSK_DOMAIN: &[u8] = b"remotecraft-psk-v1";

/// Largest plaintext a single [`SecureChannel::encrypt`] accepts. A Noise
/// message is capped at 65535 bytes including the 16-byte auth tag; callers
/// that need more must chunk. Kept well under the limit.
pub const MAX_PLAINTEXT: usize = 65_000;

/// Outbound queue depth to the relay before we treat the peer as stalled.
const OUTBOUND_QUEUE: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] remotecraft_core::ProtocolError),
    #[error("relay refused connection to {target}: {reason}")]
    Refused { target: PeerId, reason: String },
    #[error("relay error: {0}")]
    Relay(String),
    #[error("peer disconnected")]
    PeerGone,
    #[error("handshake failed (wrong password, or peer is not RemoteCraft): {0}")]
    Handshake(String),
    #[error("plaintext of {0} bytes exceeds MAX_PLAINTEXT")]
    TooLarge(usize),
    #[error("unexpected message while waiting for {expected}")]
    Unexpected { expected: &'static str },
}

/// Events from the relay that are not peer data.
#[derive(Debug, Clone)]
enum Event {
    PeerConnected { session: u64, peer: PeerId },
    PeerDisconnected,
    Refused { target: PeerId, reason: String },
    RelayError(String),
}

/// A client connection to the relay.
pub struct RelayClient {
    id: PeerId,
    tx: mpsc::Sender<ToRelay>,
    events: mpsc::Receiver<Event>,
    data: mpsc::Receiver<Vec<u8>>,
    _writer: JoinHandle<()>,
    _reader: JoinHandle<()>,
}

impl RelayClient {
    /// Connect to the relay at `relay_addr` (host:port) and register.
    /// `desired_id` asks to keep a previous ID; the relay may assign another.
    pub async fn connect(relay_addr: &str, desired_id: Option<PeerId>) -> Result<Self, NetError> {
        let stream = TcpStream::connect(relay_addr).await?;
        stream.set_nodelay(true).ok();
        let (mut reader, mut writer) = stream.into_split();

        // Register and wait for the assigned ID.
        write_frame(&mut writer, &ToRelay::Register {
            protocol_version: PROTOCOL_VERSION,
            desired_id,
        })
        .await?;
        let id = loop {
            let frame = read_frame(&mut reader).await?;
            match decode_frame::<FromRelay>(&frame)? {
                FromRelay::Registered { id } => break id,
                FromRelay::Error { message } => return Err(NetError::Relay(message)),
                _ => continue,
            }
        };

        let (out_tx, mut out_rx) = mpsc::channel::<ToRelay>(OUTBOUND_QUEUE);
        let (ev_tx, ev_rx) = mpsc::channel::<Event>(64);
        let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>(OUTBOUND_QUEUE);

        let writer_task = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                match encode_frame(&msg) {
                    Ok(bytes) => {
                        if writer.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let reader_task = tokio::spawn(async move {
            loop {
                let frame = match read_frame(&mut reader).await {
                    Ok(f) => f,
                    Err(_) => {
                        let _ = ev_tx.send(Event::PeerDisconnected).await;
                        break;
                    }
                };
                let msg: FromRelay = match decode_frame(&frame) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match msg {
                    FromRelay::Data(bytes) => {
                        if data_tx.send(bytes).await.is_err() {
                            break;
                        }
                    }
                    FromRelay::SessionOpened { session, peer } => {
                        let _ = ev_tx.send(Event::PeerConnected { session, peer }).await;
                    }
                    FromRelay::SessionClosed { .. } => {
                        let _ = ev_tx.send(Event::PeerDisconnected).await;
                    }
                    FromRelay::SessionRefused { target, reason } => {
                        let _ = ev_tx.send(Event::Refused { target, reason }).await;
                    }
                    FromRelay::Error { message } => {
                        let _ = ev_tx.send(Event::RelayError(message)).await;
                    }
                    FromRelay::Registered { .. } | FromRelay::Pong => {}
                }
            }
        });

        Ok(Self {
            id,
            tx: out_tx,
            events: ev_rx,
            data: data_rx,
            _writer: writer_task,
            _reader: reader_task,
        })
    }

    /// This client's public ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Ask the relay to pair us with `target`, and wait for the result.
    pub async fn connect_to(&mut self, target: &str) -> Result<u64, NetError> {
        self.tx
            .send(ToRelay::Connect { target: target.to_string() })
            .await
            .map_err(|_| NetError::PeerGone)?;
        match self.next_event().await? {
            Event::PeerConnected { session, .. } => Ok(session),
            Event::Refused { target, reason } => Err(NetError::Refused { target, reason }),
            Event::RelayError(m) => Err(NetError::Relay(m)),
            Event::PeerDisconnected => Err(NetError::PeerGone),
        }
    }

    /// Wait for an incoming peer (host side).
    pub async fn wait_for_peer(&mut self) -> Result<(u64, PeerId), NetError> {
        match self.next_event().await? {
            Event::PeerConnected { session, peer } => Ok((session, peer)),
            Event::RelayError(m) => Err(NetError::Relay(m)),
            Event::Refused { target, reason } => Err(NetError::Refused { target, reason }),
            Event::PeerDisconnected => Err(NetError::PeerGone),
        }
    }

    async fn next_event(&mut self) -> Result<Event, NetError> {
        self.events.recv().await.ok_or(NetError::PeerGone)
    }

    /// Send one opaque payload to the paired peer.
    pub async fn send(&self, bytes: Vec<u8>) -> Result<(), NetError> {
        self.tx.send(ToRelay::Data(bytes)).await.map_err(|_| NetError::PeerGone)
    }

    /// Receive one opaque payload from the paired peer.
    pub async fn recv(&mut self) -> Result<Vec<u8>, NetError> {
        self.data.recv().await.ok_or(NetError::PeerGone)
    }
}

/// Derive the 32-byte pre-shared key from the session password.
pub fn derive_psk(password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PSK_DOMAIN);
    hasher.update(password.as_bytes());
    let digest = hasher.finalize();
    let mut psk = [0u8; 32];
    psk.copy_from_slice(&digest);
    psk
}

/// Generate a readable random password for attended access. Uses an alphabet
/// without visually ambiguous characters (no 0/O, 1/l/I).
pub fn random_password(len: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            // `ALPHABET` is non-empty and its length fits in the range, so the
            // index is always valid; fall back to 'x' only to avoid any panic.
            let idx = rng.gen_range(0..ALPHABET.len());
            *ALPHABET.get(idx).unwrap_or(&b'x') as char
        })
        .collect()
}

/// An established end-to-end encrypted channel. Holds the Noise transport
/// state; the caller moves ciphertext over a [`RelayClient`].
pub struct SecureChannel {
    noise: snow::TransportState,
}

impl SecureChannel {
    /// Run the Noise handshake over `client` and return the secure channel.
    /// The viewer is the `initiator`; the host is the responder.
    pub async fn handshake(
        client: &mut RelayClient,
        initiator: bool,
        password: &str,
    ) -> Result<Self, NetError> {
        let psk = derive_psk(password);
        let params = NOISE_PARAMS
            .parse()
            .map_err(|e| NetError::Handshake(format!("bad noise params: {e:?}")))?;
        let builder = snow::Builder::new(params).psk(0, &psk);

        let mut hs = if initiator {
            builder.build_initiator()
        } else {
            builder.build_responder()
        }
        .map_err(|e| NetError::Handshake(format!("build: {e:?}")))?;

        let mut buf = vec![0u8; 65535];
        let mut payload = vec![0u8; 65535];

        if initiator {
            // -> e, psk
            let len = hs
                .write_message(&[], &mut buf)
                .map_err(|e| NetError::Handshake(format!("write msg1: {e:?}")))?;
            client.send(buf[..len].to_vec()).await?;
            // <- e, ee, psk
            let incoming = client.recv().await?;
            hs.read_message(&incoming, &mut payload)
                .map_err(|e| NetError::Handshake(format!("read msg2: {e:?}")))?;
        } else {
            // <- e, psk
            let incoming = client.recv().await?;
            hs.read_message(&incoming, &mut payload)
                .map_err(|e| NetError::Handshake(format!("read msg1: {e:?}")))?;
            // -> e, ee, psk
            let len = hs
                .write_message(&[], &mut buf)
                .map_err(|e| NetError::Handshake(format!("write msg2: {e:?}")))?;
            client.send(buf[..len].to_vec()).await?;
        }

        let noise = hs
            .into_transport_mode()
            .map_err(|e| NetError::Handshake(format!("transport: {e:?}")))?;
        Ok(Self { noise })
    }

    /// Encrypt one payload into a frame to send over the relay.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NetError> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(NetError::TooLarge(plaintext.len()));
        }
        let mut out = vec![0u8; plaintext.len() + 16];
        let len = self
            .noise
            .write_message(plaintext, &mut out)
            .map_err(|e| NetError::Handshake(format!("encrypt: {e:?}")))?;
        out.truncate(len);
        Ok(out)
    }

    /// Decrypt one frame received from the relay.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NetError> {
        let mut out = vec![0u8; ciphertext.len().max(16)];
        let len = self
            .noise
            .read_message(ciphertext, &mut out)
            .map_err(|e| NetError::Handshake(format!("decrypt: {e:?}")))?;
        out.truncate(len);
        Ok(out)
    }

    /// Encrypt and send a payload in one step.
    pub async fn send(&mut self, client: &RelayClient, plaintext: &[u8]) -> Result<(), NetError> {
        let ct = self.encrypt(plaintext)?;
        client.send(ct).await
    }

    /// Receive and decrypt a payload in one step.
    pub async fn recv(&mut self, client: &mut RelayClient) -> Result<Vec<u8>, NetError> {
        let ct = client.recv().await?;
        self.decrypt(&ct)
    }
}

async fn write_frame(writer: &mut OwnedWriteHalf, msg: &ToRelay) -> Result<(), NetError> {
    let frame = encode_frame(msg)?;
    writer.write_all(&frame).await?;
    Ok(())
}

async fn read_frame(reader: &mut OwnedReadHalf) -> Result<Vec<u8>, NetError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = checked_frame_len(u32::from_be_bytes(len_buf))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psk_is_deterministic_and_password_sensitive() {
        assert_eq!(derive_psk("hunter2"), derive_psk("hunter2"));
        assert_ne!(derive_psk("hunter2"), derive_psk("hunter3"));
    }

    #[test]
    fn random_password_uses_safe_alphabet() {
        let p = random_password(10);
        assert_eq!(p.len(), 10);
        assert!(p.bytes().all(|b| !b"0O1lI".contains(&b)));
    }
}
