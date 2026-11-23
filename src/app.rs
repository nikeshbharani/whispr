//! The orchestration layer.
//!
//! [`App`] ties an [`Identity`], a [`Transport`] and a [`ConnectionManager`]
//! together and exposes a small, UI-friendly surface: connect to peers, send
//! messages, and receive a stream of [`AppEvent`]s. Both the CLI and the TUI are
//! thin drivers over this type.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, Mutex};

use crate::config::{NetworkMode, TransportMode};
use crate::directory::client::DirectoryClient;
use crate::identity::Identity;
use crate::session::ConnectionManager;
use crate::transport::{self, DiscoveredPeer, Transport, TransportEvent};

/// How often to retry re-establishing a dropped session via the directory.
const RECONNECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// How long to keep trying to reconnect before giving up.
const RECONNECT_WINDOW: std::time::Duration = std::time::Duration::from_secs(180);
/// Retry cadence when redialing a dropped peer at its last known address.
const REDIAL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// How long to keep redialing a dropped peer's last known address.
const REDIAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// The peer with the larger key waits this long before redialing, so the two
/// sides don't both dial at once (the lower key goes first).
const REDIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(3);

/// Everything the UI might want to react to. All content here is already
/// decrypted and authenticated.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A peer was discovered on the network (LAN/Bluetooth modes).
    PeerDiscovered(DiscoveredPeer),
    /// A secure session with a peer was established.
    PeerConnected {
        fingerprint: String,
        display_name: String,
        /// The peer's cryptographically authenticated public key (base58). Used
        /// to reconnect the same identity via the directory after a drop.
        public_key_b58: String,
    },
    /// A peer told us its display name (arrives just after connecting).
    PeerUpdated {
        fingerprint: String,
        display_name: String,
    },
    /// A peer's session ended.
    PeerDisconnected { fingerprint: String },
    /// A chat message was received from a peer.
    Message {
        from: String,
        text: String,
        ts: u64,
        /// True if the sender broadcast this to all peers (vs. a 1:1 message).
        broadcast: bool,
    },
    /// A message we sent (identified by `id`) was acknowledged by `peer`.
    Delivered { peer: String, id: u64 },
    /// A message we sent (identified by `id`) was not acknowledged by `peer`
    /// within the delivery window (the peer likely dropped off the network).
    DeliveryFailed { peer: String, id: u64 },
    /// An informational notice.
    Info(String),
    /// A non-fatal error worth showing the user.
    Error(String),
}

/// The running application state shared by the UIs.
pub struct App {
    pub identity: Arc<Identity>,
    pub mode: TransportMode,
    pub net: NetworkMode,
    pub manager: ConnectionManager,
    pub events: mpsc::Receiver<AppEvent>,
    /// Peers currently advertised on the network, keyed by discovery id.
    pub discovered: Arc<Mutex<HashMap<String, DiscoveredPeer>>>,
    transport: Arc<dyn Transport>,
    /// Optional rendezvous directory (Internet transport only).
    directory: Option<DirectoryClient>,
}

impl App {
    /// Build the app, start the transport, and begin bridging transport events
    /// into the app event stream.
    pub async fn new(
        identity: Identity,
        mode: TransportMode,
        net: NetworkMode,
        listen_port: u16,
        directory: Option<String>,
    ) -> Result<Self> {
        let identity = Arc::new(identity);
        let (app_tx, app_rx) = mpsc::channel::<AppEvent>(256);
        let manager = ConnectionManager::new(identity.clone(), app_tx.clone());
        let transport = transport::build_transport(mode, net.clone(), listen_port, identity.clone())?;
        let discovered = Arc::new(Mutex::new(HashMap::new()));
        let directory = directory.map(DirectoryClient::new);

        // Run the transport's serve loop in the background.
        let (t_tx, mut t_rx) = mpsc::channel::<TransportEvent>(256);
        {
            let transport = transport.clone();
            let app_tx = app_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = transport.serve(t_tx).await {
                    let _ = app_tx
                        .send(AppEvent::Error(format!("transport stopped: {e}")))
                        .await;
                }
            });
        }

        // Bridge low-level transport events into high-level app events.
        {
            let manager = manager.clone();
            let app_tx = app_tx.clone();
            let discovered = discovered.clone();
            tokio::spawn(async move {
                while let Some(event) = t_rx.recv().await {
                    match event {
                        TransportEvent::Inbound { conn, .. } => {
                            let manager = manager.clone();
                            tokio::spawn(async move { manager.handle_inbound(conn).await });
                        }
                        TransportEvent::Discovered(peer) => {
                            discovered.lock().await.insert(peer.id.clone(), peer.clone());
                            let _ = app_tx.send(AppEvent::PeerDiscovered(peer)).await;
                        }
                        TransportEvent::Lost(id) => {
                            discovered.lock().await.remove(&id);
                        }
                        TransportEvent::Error(e) => {
                            let _ = app_tx.send(AppEvent::Error(e)).await;
                        }
                    }
                }
            });
        }

        // On the Internet transport, keep our address fresh in the directory so
        // peers can reach us even as our IP changes.
        if let Some(directory) = &directory {
            if matches!(mode, TransportMode::Internet) {
                if listen_port == 0 {
                    let _ = app_tx
                        .send(AppEvent::Info(
                            "set a fixed --port to auto-register with the directory".to_string(),
                        ))
                        .await;
                } else {
                    let directory = directory.clone();
                    let identity = identity.clone();
                    let app_tx = app_tx.clone();
                    tokio::spawn(async move {
                        loop {
                            match directory.register(&identity.private, listen_port).await {
                                Ok(address) => {
                                    let _ = app_tx
                                        .send(AppEvent::Info(format!(
                                            "registered with directory as {address}"
                                        )))
                                        .await;
                                }
                                Err(e) => {
                                    let _ = app_tx
                                        .send(AppEvent::Error(format!(
                                            "directory registration failed: {e}"
                                        )))
                                        .await;
                                }
                            }
                            // Re-register periodically so a new address (e.g. after
                            // a Wi-Fi/mobile switch) propagates to peers quickly.
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        }
                    });
                }
            }
        }

        Ok(Self {
            identity,
            mode,
            net,
            manager,
            events: app_rx,
            discovered,
            transport,
            directory,
        })
    }

    /// Dial a peer by raw address and establish a secure session.
    pub async fn connect(&self, addr: &str) -> Result<()> {
        let conn = self.transport.dial(addr).await?;
        self.manager.connect_conn(conn).await;
        Ok(())
    }

    /// Dial `addr` but only accept the session if the peer authenticates as
    /// `public_key_b58`. Used when (re)connecting to a discovered/known peer so a
    /// spoofed address can never impersonate them.
    pub async fn connect_expecting(&self, addr: &str, public_key_b58: &str) -> Result<()> {
        let expected = bs58::decode(public_key_b58)
            .into_vec()
            .map_err(|_| anyhow::anyhow!("invalid public key"))?;
        let conn = self.transport.dial(addr).await?;
        self.manager
            .connect_conn_expecting(conn, Some(expected))
            .await;
        Ok(())
    }

    /// Non-blocking connect: dials `addr` in the background and reports failures
    /// through the event stream. Keeps the UI responsive.
    pub fn spawn_connect(&self, addr: String) {
        let transport = self.transport.clone();
        let manager = self.manager.clone();
        let events = self.manager.events_sender();
        tokio::spawn(async move {
            match transport.dial(&addr).await {
                Ok(conn) => manager.connect_conn(conn).await,
                Err(e) => {
                    let _ = events.send(AppEvent::Error(format!("connect failed: {e}"))).await;
                }
            }
        });
    }

    /// Non-blocking, identity-verified connect (used for (re)connecting to a
    /// known/discovered peer). Only completes the session if the peer proves it
    /// owns `public_key_b58`.
    pub fn spawn_connect_expecting(&self, addr: String, public_key_b58: String) {
        let transport = self.transport.clone();
        let manager = self.manager.clone();
        let events = self.manager.events_sender();
        tokio::spawn(async move {
            let Ok(expected) = bs58::decode(&public_key_b58).into_vec() else {
                let _ = events.send(AppEvent::Error("invalid peer key".to_string())).await;
                return;
            };
            match transport.dial(&addr).await {
                Ok(conn) => manager.connect_conn_expecting(conn, Some(expected)).await,
                Err(e) => {
                    let _ = events.send(AppEvent::Error(format!("connect failed: {e}"))).await;
                }
            }
        });
    }

    /// Connect to a previously discovered peer (by discovery id or display name).
    pub async fn connect_discovered(&self, id_or_name: &str) -> Result<()> {
        let addr = {
            let discovered = self.discovered.lock().await;
            discovered
                .values()
                .find(|p| p.id == id_or_name || p.display_name == id_or_name)
                .map(|p| p.addr.clone())
        };
        let addr = addr.ok_or_else(|| anyhow::anyhow!("no such discovered peer: {id_or_name}"))?;
        self.connect(&addr).await
    }

    /// Resolve a peer's current address from the directory by their public key,
    /// then dial and connect — verifying the peer's authenticated key matches.
    pub async fn connect_by_key(&self, public_key_b58: &str) -> Result<()> {
        let directory = self
            .directory
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no directory configured (use --directory host:port)"))?;
        let expected = bs58::decode(public_key_b58)
            .into_vec()
            .map_err(|_| anyhow::anyhow!("invalid public key"))?;
        let address = directory
            .lookup(public_key_b58)
            .await?
            .ok_or_else(|| anyhow::anyhow!("peer is not registered or is offline"))?;
        let conn = self.transport.dial(&address).await?;
        self.manager
            .connect_conn_expecting(conn, Some(expected))
            .await;
        Ok(())
    }

    /// After a peer drops, keep trying to re-establish the session by looking its
    /// authenticated key up in the directory. This makes reconnection seamless
    /// across network changes (Wi-Fi <-> mobile) on the Internet transport. It is
    /// a no-op without a directory or when not on the Internet transport (LAN
    /// reconnects via mDNS rediscovery instead).
    pub fn spawn_reconnect_by_key(&self, fingerprint: String, public_key_b58: String) {
        if !matches!(self.mode, TransportMode::Internet) {
            return;
        }
        let Some(directory) = self.directory.clone() else {
            return;
        };
        let Ok(expected) = bs58::decode(&public_key_b58).into_vec() else {
            return;
        };
        let transport = self.transport.clone();
        let manager = self.manager.clone();
        let events = self.manager.events_sender();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + RECONNECT_WINDOW;
            let mut tick = tokio::time::interval(RECONNECT_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if tokio::time::Instant::now() >= deadline || manager.is_connected(&fingerprint).await
                {
                    break;
                }
                match directory.lookup(&public_key_b58).await {
                    Ok(Some(address)) => {
                        if let Ok(conn) = transport.dial(&address).await {
                            manager
                                .connect_conn_expecting(conn, Some(expected.clone()))
                                .await;
                        }
                    }
                    Ok(None) => {} // peer still offline; keep waiting for it to return
                    Err(e) => {
                        let _ = events
                            .send(AppEvent::Error(format!("reconnect lookup failed: {e}")))
                            .await;
                    }
                }
            }
        });
    }

    /// After a peer drops, keep redialing its last known address (e.g. from LAN
    /// discovery) until the session is back, instead of waiting for the next
    /// mDNS announcement or a manual Enter. Silent: failed attempts aren't
    /// reported, since the peer may simply be offline for a moment.
    pub fn spawn_redial(&self, fingerprint: String, addr: String, public_key_b58: String) {
        let Ok(expected) = bs58::decode(&public_key_b58).into_vec() else {
            return;
        };
        // Deterministic priority: only the lower key redials immediately.
        let first_delay = if self.identity.public < expected {
            std::time::Duration::ZERO
        } else {
            REDIAL_BACKOFF
        };
        let transport = self.transport.clone();
        let manager = self.manager.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + REDIAL_WINDOW;
            tokio::time::sleep(first_delay).await;
            while tokio::time::Instant::now() < deadline {
                if manager.is_connected(&fingerprint).await {
                    return;
                }
                if let Ok(conn) = transport.dial(&addr).await {
                    manager
                        .connect_conn_expecting(conn, Some(expected.clone()))
                        .await;
                    if manager.is_connected(&fingerprint).await {
                        return;
                    }
                }
                tokio::time::sleep(REDIAL_INTERVAL).await;
            }
        });
    }

    /// Send a message to a specific connected peer. Returns the message id, which
    /// later surfaces in an [`AppEvent::Delivered`]/[`AppEvent::DeliveryFailed`].
    pub async fn send(&self, fingerprint: &str, text: String) -> Result<u64> {
        self.manager.send_to(fingerprint, text).await
    }

    /// Broadcast to all connected peers. Returns the shared message id and the
    /// list of peer fingerprints it was queued to (for delivery tracking).
    pub async fn broadcast(&self, text: String) -> (u64, Vec<String>) {
        self.manager.broadcast(text).await
    }

    /// Gracefully say goodbye to a peer so it flushes and closes cleanly. Used by
    /// one-shot clients to guarantee delivery of the final message before exit.
    pub async fn disconnect(&self, fingerprint: &str) {
        self.manager.disconnect(fingerprint).await
    }

    /// Whether the active transport can discover peers autonomously.
    pub fn supports_discovery(&self) -> bool {
        self.transport.supports_discovery()
    }
}
