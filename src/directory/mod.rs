//! Optional rendezvous **directory server** for the Internet transport only.
//!
//! LAN and Bluetooth stay fully serverless (mDNS / BLE handle discovery). Over
//! the internet, though, IP addresses change, so peers need a way to answer
//! *"where is key X right now?"*. The directory is a tiny, stateless-ish service
//! that stores `public key -> current address` records with a short TTL.
//!
//! It is designed to be **untrusted**:
//! - Every client authenticates to the server with the same Noise handshake used
//!   between peers, so registering a record *proves ownership* of that key.
//! - Messaging never flows through the server; it only ever learns addresses.
//! - When you dial a peer looked up here, the peer-to-peer Noise handshake
//!   re-verifies the peer's key, and the client rejects any mismatch — so a
//!   malicious directory can cause a failed connection but never impersonation.

pub mod client;
pub mod server;

use serde::{Deserialize, Serialize};

/// Default port the directory server listens on.
pub const DEFAULT_PORT: u16 = 6060;

/// A request from a client to the directory server (sent encrypted).
#[derive(Debug, Serialize, Deserialize)]
pub enum DirRequest {
    /// Register the caller (identified by its handshake key) at `port`; the
    /// server combines it with the caller's observed IP.
    Register { port: u16 },
    /// Look up the current address of the peer with this public key.
    Lookup { public_key_b58: String },
    /// Liveness check.
    Ping,
}

/// A response from the directory server (sent encrypted).
#[derive(Debug, Serialize, Deserialize)]
pub enum DirResponse {
    /// Registration succeeded; echoes the address the server recorded.
    Registered { address: String },
    /// A live record was found.
    Found { address: String },
    /// No live record for that key.
    NotFound,
    /// Reply to [`DirRequest::Ping`].
    Pong,
    /// The request could not be processed.
    Error(String),
}
