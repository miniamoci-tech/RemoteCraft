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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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

/// Largest logical message [`SecureChannel::send_message`] accepts, in bytes.
/// Bounds the buffer a hostile peer can make the receiver reassemble. A
/// generous cap for full-screen JPEG frames.
pub const MAX_MESSAGE: usize = 32 * 1024 * 1024;

/// Outbound queue depth to the relay before we treat the peer as stalled.
const OUTBOUND_QUEUE: usize = 1024;

/// Maximum time either peer may spend authenticating a session.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

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
    #[error("authentication timed out after 20 seconds")]
    HandshakeTimeout,
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

/// Session boundaries travel with data so a pending read wakes on disconnect
/// and a subsequent handshake can discard bytes queued by the previous peer.
struct PeerData {
    session: u64,
    bytes: Option<Vec<u8>>,
}

/// A client connection to the relay.
///
/// All methods take `&self` so the client can be shared (behind an `Arc`)
/// between a task that sends and one that receives — the host streams screen
/// frames while receiving input events at the same time. The inbound channels
/// are each behind a `Mutex` because an `mpsc::Receiver` needs `&mut` to poll;
/// only one task receives from each at a time.
pub struct RelayClient {
    id: PeerId,
    session: AtomicU64,
    tx: mpsc::Sender<ToRelay>,
    events: tokio::sync::Mutex<mpsc::Receiver<Event>>,
    data: tokio::sync::Mutex<mpsc::Receiver<PeerData>>,
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
        write_frame(
            &mut writer,
            &ToRelay::Register {
                protocol_version: PROTOCOL_VERSION,
                desired_id,
            },
        )
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
        let (data_tx, data_rx) = mpsc::channel::<PeerData>(OUTBOUND_QUEUE);

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
                    FromRelay::Data { session, bytes } => {
                        if data_tx
                            .send(PeerData {
                                session,
                                bytes: Some(bytes),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    FromRelay::SessionOpened { session, peer } => {
                        let _ = ev_tx.send(Event::PeerConnected { session, peer }).await;
                    }
                    FromRelay::SessionClosed { session } => {
                        if data_tx
                            .send(PeerData {
                                session,
                                bytes: None,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
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
            session: AtomicU64::new(0),
            tx: out_tx,
            events: tokio::sync::Mutex::new(ev_rx),
            data: tokio::sync::Mutex::new(data_rx),
            _writer: writer_task,
            _reader: reader_task,
        })
    }

    /// This client's public ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Ask the relay to pair us with `target`, and wait for the result.
    pub async fn connect_to(&self, target: &str) -> Result<u64, NetError> {
        self.tx
            .send(ToRelay::Connect {
                target: target.to_string(),
            })
            .await
            .map_err(|_| NetError::PeerGone)?;
        loop {
            match self.next_event().await? {
                Event::PeerConnected { session, .. } => {
                    self.session.store(session, Ordering::Relaxed);
                    return Ok(session);
                }
                Event::Refused { target, reason } => {
                    return Err(NetError::Refused { target, reason })
                }
                Event::RelayError(m) => return Err(NetError::Relay(m)),
                Event::PeerDisconnected => continue,
            }
        }
    }

    /// Wait for an incoming peer (host side).
    pub async fn wait_for_peer(&self) -> Result<(u64, PeerId), NetError> {
        let mut events = self.events.lock().await;
        let mut data = self.data.lock().await;
        loop {
            // Drain abandoned session data while waiting. Otherwise a peer
            // that queued data before failing authentication can block the
            // reader from delivering the close/new-session notifications.
            let event = tokio::select! {
                biased;
                event = events.recv() => event.ok_or(NetError::PeerGone)?,
                frame = data.recv() => {
                    if frame.is_none() {
                        return Err(NetError::PeerGone);
                    }
                    continue;
                }
            };
            match event {
                Event::PeerConnected { session, peer } => {
                    self.session.store(session, Ordering::Relaxed);
                    return Ok((session, peer));
                }
                Event::RelayError(m) => return Err(NetError::Relay(m)),
                Event::Refused { target, reason } => {
                    return Err(NetError::Refused { target, reason })
                }
                Event::PeerDisconnected => continue,
            }
        }
    }

    async fn next_event(&self) -> Result<Event, NetError> {
        let mut events = self.events.lock().await;
        events.recv().await.ok_or(NetError::PeerGone)
    }

    /// Send one opaque payload to the paired peer.
    pub async fn send(&self, bytes: Vec<u8>) -> Result<(), NetError> {
        let session = self.session.load(Ordering::Relaxed);
        if session == 0 {
            return Err(NetError::PeerGone);
        }
        self.tx
            .send(ToRelay::Data { session, bytes })
            .await
            .map_err(|_| NetError::PeerGone)
    }

    /// Receive one opaque payload from the paired peer.
    pub async fn recv(&self) -> Result<Vec<u8>, NetError> {
        let session = self.session.load(Ordering::Relaxed);
        if session == 0 {
            return Err(NetError::PeerGone);
        }
        let mut data = self.data.lock().await;
        while let Some(frame) = data.recv().await {
            if frame.session == session {
                return frame.bytes.ok_or(NetError::PeerGone);
            }
        }
        Err(NetError::PeerGone)
    }

    /// Release a session while retaining this peer's public ID.
    pub async fn disconnect(&self, session: u64) -> Result<(), NetError> {
        self.tx
            .send(ToRelay::Disconnect { session })
            .await
            .map_err(|_| NetError::PeerGone)
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

/// An established end-to-end encrypted channel.
///
/// Uses Noise in *stateless* transport mode: the cipher state is immutable and
/// each message carries an explicit nonce, which we draw from two independent
/// monotonic counters (one per direction). That lets the channel be cloned and
/// used from two tasks at once — one encrypting outbound frames, one decrypting
/// inbound input — without a lock on the cipher. Each direction is a single
/// ordered stream, so its nonces stay in lockstep with the peer's.
#[derive(Clone)]
pub struct SecureChannel {
    transport: Arc<snow::StatelessTransportState>,
    send_nonce: Arc<AtomicU64>,
    recv_nonce: Arc<AtomicU64>,
}

impl SecureChannel {
    /// Run the Noise handshake over `client` and return the secure channel.
    /// The viewer is the `initiator`; the host is the responder.
    /// Authentication is bounded by [`HANDSHAKE_TIMEOUT`]. A failure requests
    /// session closure so a silent or invalid peer cannot keep the host busy.
    pub async fn handshake(
        client: &RelayClient,
        initiator: bool,
        password: &str,
    ) -> Result<Self, NetError> {
        let session = client.session.load(Ordering::Relaxed);
        let result = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            Self::handshake_inner(client, initiator, password),
        )
        .await
        .unwrap_or(Err(NetError::HandshakeTimeout));
        if result.is_err() {
            // A failed or silent peer must not keep the host marked busy.
            let _ = client.disconnect(session).await;
        }
        result
    }

    async fn handshake_inner(
        client: &RelayClient,
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
            let len = hs
                .write_message(&[], &mut buf)
                .map_err(|e| NetError::Handshake(format!("write msg1: {e:?}")))?;
            client.send(buf[..len].to_vec()).await?;
            let incoming = client.recv().await?;
            hs.read_message(&incoming, &mut payload)
                .map_err(|e| NetError::Handshake(format!("read msg2: {e:?}")))?;
        } else {
            let incoming = client.recv().await?;
            hs.read_message(&incoming, &mut payload)
                .map_err(|e| NetError::Handshake(format!("read msg1: {e:?}")))?;
            let len = hs
                .write_message(&[], &mut buf)
                .map_err(|e| NetError::Handshake(format!("write msg2: {e:?}")))?;
            client.send(buf[..len].to_vec()).await?;
        }

        let transport = hs
            .into_stateless_transport_mode()
            .map_err(|e| NetError::Handshake(format!("transport: {e:?}")))?;
        Ok(Self {
            transport: Arc::new(transport),
            send_nonce: Arc::new(AtomicU64::new(0)),
            recv_nonce: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Encrypt one payload into a frame to send over the relay.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, NetError> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(NetError::TooLarge(plaintext.len()));
        }
        let nonce = self.send_nonce.fetch_add(1, Ordering::Relaxed);
        let mut out = vec![0u8; plaintext.len() + 16];
        let len = self
            .transport
            .write_message(nonce, plaintext, &mut out)
            .map_err(|e| NetError::Handshake(format!("encrypt: {e:?}")))?;
        out.truncate(len);
        Ok(out)
    }

    /// Decrypt one frame received from the relay.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, NetError> {
        let nonce = self.recv_nonce.fetch_add(1, Ordering::Relaxed);
        let mut out = vec![0u8; ciphertext.len().max(16)];
        let len = self
            .transport
            .read_message(nonce, ciphertext, &mut out)
            .map_err(|e| NetError::Handshake(format!("decrypt: {e:?}")))?;
        out.truncate(len);
        Ok(out)
    }

    /// Encrypt and send a payload in one step.
    pub async fn send(&self, client: &RelayClient, plaintext: &[u8]) -> Result<(), NetError> {
        let ct = self.encrypt(plaintext)?;
        client.send(ct).await
    }

    /// Receive and decrypt a payload in one step.
    pub async fn recv(&self, client: &RelayClient) -> Result<Vec<u8>, NetError> {
        let ct = client.recv().await?;
        self.decrypt(&ct)
    }

    /// Send a logical message of any size, split across as many encrypted
    /// Noise messages as needed. The first chunk carries a 4-byte big-endian
    /// total length; the receiver reassembles with [`recv_message`].
    ///
    /// [`recv_message`]: Self::recv_message
    pub async fn send_message(&self, client: &RelayClient, data: &[u8]) -> Result<(), NetError> {
        if data.len() > MAX_MESSAGE {
            return Err(NetError::TooLarge(data.len()));
        }
        // data.len() <= MAX_MESSAGE which fits in u32.
        let total = (data.len() as u32).to_be_bytes();
        let head = data.len().min(MAX_PLAINTEXT - 4);
        let mut first = Vec::with_capacity(4 + head);
        first.extend_from_slice(&total);
        first.extend_from_slice(&data[..head]);
        self.send(client, &first).await?;

        let mut offset = head;
        while offset < data.len() {
            let end = (offset + MAX_PLAINTEXT).min(data.len());
            self.send(client, &data[offset..end]).await?;
            offset = end;
        }
        Ok(())
    }

    /// Receive a logical message sent with [`send_message`].
    ///
    /// [`send_message`]: Self::send_message
    pub async fn recv_message(&self, client: &RelayClient) -> Result<Vec<u8>, NetError> {
        let first = self.recv(client).await?;
        if first.len() < 4 {
            return Err(NetError::Unexpected {
                expected: "message length prefix",
            });
        }
        let total = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
        if total > MAX_MESSAGE {
            return Err(NetError::TooLarge(total));
        }
        let mut acc = Vec::with_capacity(total);
        acc.extend_from_slice(&first[4..]);
        while acc.len() < total {
            let chunk = self.recv(client).await?;
            acc.extend_from_slice(&chunk);
            if acc.len() > total {
                acc.truncate(total);
                break;
            }
        }
        Ok(acc)
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
