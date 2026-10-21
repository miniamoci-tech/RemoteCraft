//! RemoteCraft relay (rendezvous) server.
//!
//! The relay is a dumb, privacy-preserving broker. Peers connect over TCP,
//! register to receive a short numeric ID, and ask to be paired with another
//! ID. Once two peers are paired the relay forwards opaque, already-encrypted
//! bytes between them and never inspects the contents. It stores nothing on
//! disk and keeps no history.
//!
//! Run it on a small Linux box (see `deploy/`). It is the only component that
//! needs a public address; both desktop peers dial out to it, so neither PC
//! needs an inbound port opened on its router.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use remotecraft_core::{
    checked_frame_len, decode_frame, encode_frame, FromRelay, PeerId, ProtocolError, ToRelay,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// How many outbound frames may queue for a peer before we consider it stalled
/// and drop the connection. Bounds memory per peer.
const OUTBOUND_QUEUE: usize = 256;

/// One registered peer: a channel to its writer task plus its current pairing.
struct PeerEntry {
    tx: mpsc::Sender<FromRelay>,
    partner: Option<PeerId>,
    session: Option<u64>,
}

/// Shared relay state. Cheap to clone (an `Arc`).
#[derive(Clone)]
pub struct Relay {
    peers: Arc<Mutex<HashMap<PeerId, PeerEntry>>>,
    next_session: Arc<AtomicU64>,
}

impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}

impl Relay {
    pub fn new() -> Self {
        Self {
            peers: Arc::new(Mutex::new(HashMap::new())),
            next_session: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Serve forever on an already-bound listener. Each connection is handled
    /// on its own task; a failure on one connection never affects others.
    pub async fn serve(self, listener: TcpListener) -> std::io::Result<()> {
        loop {
            let (socket, addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(error = %e, "accept failed");
                    continue;
                }
            };
            let relay = self.clone();
            tokio::spawn(async move {
                if let Err(e) = relay.handle_connection(socket).await {
                    info!(peer = %addr, error = %e, "connection closed");
                }
            });
        }
    }

    async fn handle_connection(&self, socket: TcpStream) -> anyhow::Result<()> {
        let (mut reader, mut writer) = socket.into_split();

        // The first frame must be Register.
        let first = read_frame(&mut reader).await?;
        let msg: ToRelay = decode_frame(&first)?;
        let desired = match msg {
            ToRelay::Register {
                protocol_version,
                desired_id,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    let body = encode_frame(&FromRelay::Error {
                        message: format!(
                            "unsupported protocol version {protocol_version}, relay speaks {PROTOCOL_VERSION}"
                        ),
                    })?;
                    writer.write_all(&body).await?;
                    return Ok(());
                }
                desired_id
            }
            _ => {
                let body = encode_frame(&FromRelay::Error {
                    message: "first message must be Register".to_string(),
                })?;
                writer.write_all(&body).await?;
                return Ok(());
            }
        };

        let (tx, mut rx) = mpsc::channel::<FromRelay>(OUTBOUND_QUEUE);
        let id = self.register(desired, tx)?;
        info!(id = %id, "peer registered");

        // Writer task: drain outbound queue to the socket.
        let writer_id = id.clone();
        let writer_task = tokio::spawn(async move {
            // Confirm registration first.
            if let Ok(body) = encode_frame(&FromRelay::Registered {
                id: writer_id.clone(),
            }) {
                if writer.write_all(&body).await.is_err() {
                    return;
                }
            }
            while let Some(out) = rx.recv().await {
                match encode_frame(&out) {
                    Ok(body) => {
                        if writer.write_all(&body).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(id = %writer_id, error = %e, "drop unencodable frame");
                    }
                }
            }
        });

        // Reader loop.
        let result = self.read_loop(&id, &mut reader).await;

        // Cleanup: remove this peer and notify any partner.
        self.deregister(&id);
        writer_task.abort();
        result
    }

    async fn read_loop(
        &self,
        id: &PeerId,
        reader: &mut (impl AsyncReadExt + Unpin),
    ) -> anyhow::Result<()> {
        loop {
            let frame = match read_frame(reader).await {
                Ok(f) => f,
                Err(_) => return Ok(()), // peer hung up
            };
            let msg: ToRelay = match decode_frame(&frame) {
                Ok(m) => m,
                Err(e) => {
                    warn!(id = %id, error = %e, "bad frame from peer");
                    continue;
                }
            };
            match msg {
                ToRelay::Register { .. } => {
                    self.send_to(id, FromRelay::Error {
                        message: "already registered".to_string(),
                    });
                }
                ToRelay::Ping => {
                    self.send_to(id, FromRelay::Pong);
                }
                ToRelay::Connect { target } => {
                    self.open_session(id, &target);
                }
                ToRelay::Data(bytes) => {
                    self.forward_data(id, bytes);
                }
            }
        }
    }

    /// Register a peer, assigning an ID. Honours `desired` when free.
    fn register(&self, desired: Option<PeerId>, tx: mpsc::Sender<FromRelay>) -> anyhow::Result<PeerId> {
        let mut peers = lock(&self.peers)?;
        let id = match desired {
            Some(d) if is_valid_id(&d) && !peers.contains_key(&d) => d,
            _ => {
                let mut candidate = generate_id();
                let mut tries = 0;
                while peers.contains_key(&candidate) {
                    candidate = generate_id();
                    tries += 1;
                    if tries > 1000 {
                        anyhow::bail!("could not allocate a free id");
                    }
                }
                candidate
            }
        };
        peers.insert(
            id.clone(),
            PeerEntry {
                tx,
                partner: None,
                session: None,
            },
        );
        Ok(id)
    }

    fn deregister(&self, id: &PeerId) {
        let partner = {
            let mut peers = match self.peers.lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            let partner = peers.get(id).and_then(|e| e.partner.clone());
            peers.remove(id);
            // Clear the partner's back-reference.
            if let Some(ref p) = partner {
                if let Some(entry) = peers.get_mut(p) {
                    let session = entry.session;
                    entry.partner = None;
                    entry.session = None;
                    if let Some(session) = session {
                        let _ = entry.tx.try_send(FromRelay::SessionClosed { session });
                    }
                }
            }
            partner
        };
        if let Some(p) = partner {
            info!(id = %id, partner = %p, "session torn down");
        }
    }

    fn open_session(&self, from: &PeerId, target: &PeerId) {
        let mut peers = match self.peers.lock() {
            Ok(p) => p,
            Err(poisoned) => poisoned.into_inner(),
        };

        if target == from {
            drop(peers);
            self.send_to(from, FromRelay::SessionRefused {
                target: target.clone(),
                reason: "cannot connect to yourself".to_string(),
            });
            return;
        }
        if !peers.contains_key(target) {
            drop(peers);
            self.send_to(from, FromRelay::SessionRefused {
                target: target.clone(),
                reason: "peer is offline or unknown".to_string(),
            });
            return;
        }
        // Reject if either side is already in a session.
        let from_busy = peers.get(from).map(|e| e.partner.is_some()).unwrap_or(true);
        let target_busy = peers.get(target).map(|e| e.partner.is_some()).unwrap_or(true);
        if from_busy || target_busy {
            drop(peers);
            self.send_to(from, FromRelay::SessionRefused {
                target: target.clone(),
                reason: "one of the peers is already in a session".to_string(),
            });
            return;
        }

        let session = self.next_session.fetch_add(1, Ordering::Relaxed);
        if let Some(e) = peers.get_mut(from) {
            e.partner = Some(target.clone());
            e.session = Some(session);
        }
        if let Some(e) = peers.get_mut(target) {
            e.partner = Some(from.clone());
            e.session = Some(session);
        }
        let from_tx = peers.get(from).map(|e| e.tx.clone());
        let target_tx = peers.get(target).map(|e| e.tx.clone());
        drop(peers);

        if let Some(tx) = from_tx {
            let _ = tx.try_send(FromRelay::SessionOpened {
                session,
                peer: target.clone(),
            });
        }
        if let Some(tx) = target_tx {
            let _ = tx.try_send(FromRelay::SessionOpened {
                session,
                peer: from.clone(),
            });
        }
        info!(from = %from, target = %target, session, "session opened");
    }

    fn forward_data(&self, from: &PeerId, bytes: Vec<u8>) {
        let target_tx = {
            let peers = match self.peers.lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            peers
                .get(from)
                .and_then(|e| e.partner.clone())
                .and_then(|p| peers.get(&p).map(|e| e.tx.clone()))
        };
        if let Some(tx) = target_tx {
            // Drop the frame if the partner is backed up rather than blocking
            // the whole relay; a stalled viewer should not stall the host.
            let _ = tx.try_send(FromRelay::Data(bytes));
        }
    }

    fn send_to(&self, id: &PeerId, msg: FromRelay) {
        let peers = match self.peers.lock() {
            Ok(p) => p,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entry) = peers.get(id) {
            let _ = entry.tx.try_send(msg);
        }
    }

    /// Number of currently registered peers (for tests and metrics).
    pub fn peer_count(&self) -> usize {
        match self.peers.lock() {
            Ok(p) => p.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> anyhow::Result<std::sync::MutexGuard<'_, T>> {
    m.lock().map_err(|_| anyhow::anyhow!("relay state lock poisoned"))
}

/// A valid public ID is exactly nine ASCII digits.
fn is_valid_id(id: &str) -> bool {
    id.len() == 9 && id.bytes().all(|b| b.is_ascii_digit())
}

/// Generate a pseudo-random nine-digit ID. Not cryptographic: IDs are public
/// addresses, not secrets. Security comes from the session password, not from
/// the ID being hard to guess.
fn generate_id() -> PeerId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix the clock with a counter-ish hash so rapid successive calls differ.
    let mut x = nanos as u64 ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    let n = x % 1_000_000_000;
    format!("{n:09}")
}

/// Read one length-prefixed frame, bounding the allocation to [`MAX_FRAME_LEN`].
async fn read_frame(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<Vec<u8>, FrameReadError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let declared = u32::from_be_bytes(len_buf);
    let len = checked_frame_len(declared)?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

#[derive(Debug, thiserror::Error)]
enum FrameReadError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_validation() {
        assert!(is_valid_id("123456789"));
        assert!(!is_valid_id("12345678")); // too short
        assert!(!is_valid_id("1234567890")); // too long
        assert!(!is_valid_id("12345678a")); // non-digit
    }

    #[test]
    fn generated_ids_are_valid() {
        for _ in 0..100 {
            let id = generate_id();
            assert!(is_valid_id(&id), "invalid generated id: {id}");
        }
    }
}
