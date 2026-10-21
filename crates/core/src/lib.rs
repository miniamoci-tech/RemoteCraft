//! Shared protocol types for RemoteCraft.
//!
//! The relay server is a rendezvous point: each peer keeps one control
//! connection open and is addressed by a short numeric ID. A connecting peer
//! asks the relay to broker a session to a target ID; the relay then forwards
//! opaque bytes between the two peers for the lifetime of that session.
//!
//! This crate carries only the framing and the control messages. It knows
//! nothing about screens, input or encryption — those live in higher layers.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use serde::{Deserialize, Serialize};

/// Protocol version. Peers and relay must agree on the major value.
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest frame payload the relay or a peer will accept, in bytes.
///
/// Frames are length-prefixed; this cap bounds the allocation a remote peer
/// can force on us, so a malformed or hostile length never triggers a huge
/// `Vec` reservation.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// A peer's public address on the relay: a short numeric string such as
/// `"123 456 789"` without the spaces. Assigned by the relay on registration.
pub type PeerId = String;

/// Errors from framing and (de)serialisation.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame length {0} exceeds maximum {MAX_FRAME_LEN}")]
    FrameTooLarge(usize),
    #[error("failed to encode message: {0}")]
    Encode(String),
    #[error("failed to decode message: {0}")]
    Decode(String),
}

/// Control messages a peer sends to the relay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToRelay {
    /// First message on a control connection. `desired_id` lets a peer ask to
    /// keep the ID it was given before; the relay may assign a different one.
    Register {
        protocol_version: u16,
        desired_id: Option<PeerId>,
    },
    /// Ask the relay to broker a session to `target`. The relay replies with
    /// [`FromRelay::SessionOpened`] or [`FromRelay::SessionRefused`].
    Connect { target: PeerId },
    /// Keep the control connection alive through NAT/idle timeouts.
    Ping,
    /// Opaque payload for the peer's current session. The relay forwards the
    /// bytes verbatim to the paired peer and never inspects them.
    Data(Vec<u8>),
}

/// Control messages the relay sends to a peer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FromRelay {
    /// Registration accepted; `id` is this peer's public address.
    Registered { id: PeerId },
    /// A session was brokered. Both peers receive this with the same
    /// `session`; subsequent relayed bytes are tagged with it.
    SessionOpened { session: u64, peer: PeerId },
    /// The requested target is unknown, offline or declined.
    SessionRefused { target: PeerId, reason: String },
    /// Relay could not honour the request (bad version, etc.).
    Error { message: String },
    /// Reply to [`ToRelay::Ping`].
    Pong,
    /// Opaque payload forwarded from the paired peer's [`ToRelay::Data`].
    Data(Vec<u8>),
    /// The paired peer disconnected or ended the session.
    SessionClosed { session: u64 },
}

/// Encode a value into a length-prefixed frame (4-byte big-endian length
/// followed by the bincode payload).
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let body = bincode::serialize(value).map_err(|e| ProtocolError::Encode(e.to_string()))?;
    if body.len() > MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge(body.len()));
    }
    // `body.len()` is bounded by MAX_FRAME_LEN above, which fits in u32.
    let len = body.len() as u32;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Validate a declared frame length read from the wire before allocating.
///
/// Returns the length as `usize` when it is within [`MAX_FRAME_LEN`], or an
/// error otherwise, so callers never reserve an attacker-chosen buffer.
pub fn checked_frame_len(declared: u32) -> Result<usize, ProtocolError> {
    let len = declared as usize;
    if len > MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    Ok(len)
}

/// Decode a value from a frame payload (without the length prefix).
pub fn decode_frame<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, ProtocolError> {
    bincode::deserialize(body).map_err(|e| ProtocolError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_register() {
        let msg = ToRelay::Register {
            protocol_version: PROTOCOL_VERSION,
            desired_id: Some("123456789".to_string()),
        };
        let frame = encode_frame(&msg).expect("encode");
        // Strip the 4-byte length prefix.
        let declared = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
        let len = checked_frame_len(declared).expect("len ok");
        assert_eq!(len, frame.len() - 4);
        let decoded: ToRelay = decode_frame(&frame[4..]).expect("decode");
        assert_eq!(decoded, msg);
    }

    #[test]
    fn oversized_length_is_rejected_without_allocating() {
        let err = checked_frame_len(u32::MAX).expect_err("should reject");
        assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
    }

    #[test]
    fn decode_garbage_is_error_not_panic() {
        let result: Result<FromRelay, _> = decode_frame(&[0xff, 0xff, 0xff, 0xff]);
        assert!(result.is_err());
    }
}
