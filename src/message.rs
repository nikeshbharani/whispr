//! The wire protocol exchanged between two peers *after* the Noise handshake.
//!
//! Every value is serialised with JSON and then sealed inside a Noise transport
//! message, so the bytes on the wire are always encrypted. JSON keeps the
//! protocol easy to read and evolve; the volume is tiny.

use serde::{Deserialize, Serialize};

/// A single application-level message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Protocol {
    /// Sent immediately after the handshake to announce a display name and let
    /// the peer double-check the static key it just authenticated.
    Hello {
        display_name: String,
        public_key_b58: String,
    },
    /// A chat message to a single peer. `id` is a per-sender identifier the
    /// recipient echoes back in an [`Protocol::Ack`] so delivery can be confirmed.
    Text { id: u64, body: String, ts: u64 },
    /// A chat message sent to every connected peer at once. Carried as a
    /// distinct variant so the receiver can file it under its Broadcast channel
    /// rather than the sender's one-to-one conversation.
    Broadcast { id: u64, body: String, ts: u64 },
    /// Confirms receipt of the [`Protocol::Text`]/[`Protocol::Broadcast`] with
    /// this `id`, so the sender can show a delivery receipt.
    Ack { id: u64 },
    /// Liveness probe (sent periodically). The peer replies with [`Protocol::Pong`].
    Ping,
    /// Reply to a [`Protocol::Ping`].
    Pong,
    /// A graceful goodbye before closing the connection.
    Bye,
}

/// Current unix time in whole seconds (0 if the clock is before the epoch).
pub fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
