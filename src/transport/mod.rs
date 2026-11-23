//! Pluggable transports.
//!
//! A [`Transport`] is the lowest layer: it knows how to *find* peers and how to
//! open raw, unencrypted byte streams to them. Everything above it (the Noise
//! handshake, framing, sessions) is transport-agnostic, so adding a new medium
//! is just a matter of implementing this one trait.
//!
//! Three transports ship with whispr:
//! - [`lan`]       — mDNS auto-discovery + TCP on the local network (fully working).
//! - [`internet`]  — TCP over the internet, optionally via Tor or a SOCKS5 proxy.
//! - [`bluetooth`] — Bluetooth Low Energy (experimental scaffold, feature-gated).

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::config::{NetworkMode, TransportMode};
use crate::identity::Identity;

pub mod bluetooth;
pub mod internet;
pub mod lan;

/// Any bidirectional, byte-oriented stream a transport can produce.
pub trait RawStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> RawStream for T {}

/// A boxed raw connection handed up to the session layer for the handshake.
pub type RawConn = Box<dyn RawStream>;

/// A peer advertised by a discovery-capable transport (e.g. mDNS on the LAN).
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// A stable id for this advertisement (used to de-duplicate).
    pub id: String,
    /// The peer's chosen display name, if advertised.
    pub display_name: String,
    /// An address this peer can be dialed at via [`Transport::dial`].
    pub addr: String,
    /// The peer's advertised public key, if any (for out-of-band verification).
    pub public_key_b58: Option<String>,
}

/// Events surfaced by a running transport.
pub enum TransportEvent {
    /// A peer was discovered (or re-advertised).
    Discovered(DiscoveredPeer),
    /// A previously discovered peer went away.
    Lost(String),
    /// An inbound connection was accepted and is ready to be handshaken.
    Inbound {
        conn: RawConn,
        from: Option<SocketAddr>,
    },
    /// A non-fatal transport error worth surfacing to the user.
    Error(String),
}

/// The behaviour every transport must provide.
#[async_trait]
pub trait Transport: Send + Sync {
    /// A short machine name, e.g. `"lan"`.
    fn name(&self) -> &'static str;

    /// Whether this transport can autonomously discover peers.
    fn supports_discovery(&self) -> bool {
        false
    }

    /// Start listening for inbound connections and (if supported) discovering
    /// peers. Runs until the event receiver is dropped or an error occurs.
    async fn serve(&self, events: mpsc::Sender<TransportEvent>) -> anyhow::Result<()>;

    /// Dial a peer at `addr`, returning a raw (not-yet-encrypted) connection.
    async fn dial(&self, addr: &str) -> anyhow::Result<RawConn>;
}

/// Construct the transport selected by the user.
pub fn build_transport(
    mode: TransportMode,
    net: NetworkMode,
    listen_port: u16,
    identity: Arc<Identity>,
) -> anyhow::Result<Arc<dyn Transport>> {
    match mode {
        TransportMode::Lan => Ok(Arc::new(lan::LanTransport::new(listen_port, identity)?)),
        TransportMode::Internet => Ok(Arc::new(internet::InternetTransport::new(listen_port, net)?)),
        TransportMode::Bluetooth => bluetooth::build(identity),
    }
}
