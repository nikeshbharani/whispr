//! Connection and peer management.
//!
//! This layer is transport-agnostic. Given a raw connection (from either an
//! inbound accept or an outbound dial) it performs the Noise handshake, learns
//! the peer's verified identity, and then runs a dedicated task that encrypts
//! outbound messages and decrypts inbound ones. The rest of the app only ever
//! sees decrypted [`AppEvent`]s and sends plaintext through the manager.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use snow::TransportState;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, MissedTickBehavior};

use crate::app::AppEvent;
use crate::crypto::noise;
use crate::identity::{fingerprint_of, Identity};
use crate::message::{now_ts, Protocol};
use crate::transport::RawConn;

/// How often each peer task sends a keepalive ping.
const HEARTBEAT: Duration = Duration::from_secs(4);
/// A peer is considered gone if no frame arrives within this window (covers
/// silent drops such as switching Wi-Fi off or roaming to mobile data). Kept
/// short so a network change is noticed quickly and reconnection can begin.
const PEER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for a message to be acknowledged before reporting it as
/// undelivered. Generous so a slow path (e.g. Tor) doesn't cause false alarms;
/// a successful ack resolves instantly regardless.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the peer task checks for messages that have missed their ack.
const ACK_SWEEP: Duration = Duration::from_secs(2);
/// Two sessions to the same peer established within this window are treated as
/// a simultaneous-dial duplicate (both sides dialed at once). Both peers then
/// keep the same one — the session with the smaller handshake hash.
const DUPLICATE_WINDOW: Duration = Duration::from_secs(5);
/// When a young session closes, wait this long before declaring the peer gone,
/// so a duplicate being replaced by its winner doesn't flicker "disconnected".
const DUPLICATE_GRACE: Duration = Duration::from_millis(1500);

/// A currently-connected peer and the channel used to send it messages.
struct ConnectedPeer {
    /// Distinguishes reconnections so a stale task can't evict a fresh session.
    session_id: u64,
    display_name: String,
    outbound: mpsc::Sender<Protocol>,
    /// Noise handshake hash (same on both ends); used to break duplicate ties.
    handshake_hash: Vec<u8>,
    established: Instant,
}

/// Manages the set of live peer connections. Cheap to clone (everything shared
/// state lives behind an `Arc`).
#[derive(Clone)]
pub struct ConnectionManager {
    identity: Arc<Identity>,
    peers: Arc<Mutex<HashMap<String, ConnectedPeer>>>,
    events: mpsc::Sender<AppEvent>,
    next_session: Arc<AtomicU64>,
    next_msg_id: Arc<AtomicU64>,
}

impl ConnectionManager {
    pub fn new(identity: Arc<Identity>, events: mpsc::Sender<AppEvent>) -> Self {
        Self {
            identity,
            peers: Arc::new(Mutex::new(HashMap::new())),
            events,
            next_session: Arc::new(AtomicU64::new(1)),
            next_msg_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Handshake an inbound connection as the responder, then start serving it.
    pub async fn handle_inbound(&self, mut conn: RawConn) {
        match noise::handshake_responder(&mut conn, &self.identity.private).await {
            Ok(hs) => {
                self.start_peer(hs.transport, conn, hs.remote_public, hs.handshake_hash)
                    .await
            }
            Err(e) => {
                let _ = self
                    .events
                    .send(AppEvent::Error(format!("inbound handshake failed: {e}")))
                    .await;
            }
        }
    }

    /// Handshake an outbound connection as the initiator, then start serving it.
    pub async fn connect_conn(&self, conn: RawConn) {
        self.connect_conn_expecting(conn, None).await;
    }

    /// Like [`connect_conn`](Self::connect_conn) but, when `expected` is set,
    /// aborts unless the peer's authenticated key matches it. This is what makes
    /// dialing an address from the (untrusted) directory safe: a wrong address
    /// fails cleanly instead of connecting us to an impostor.
    pub async fn connect_conn_expecting(&self, mut conn: RawConn, expected: Option<Vec<u8>>) {
        match noise::handshake_initiator(&mut conn, &self.identity.private).await {
            Ok(hs) => {
                if let Some(expected) = expected {
                    if expected != hs.remote_public {
                        let _ = self
                            .events
                            .send(AppEvent::Error(
                                "peer key did not match the requested identity — aborting"
                                    .to_string(),
                            ))
                            .await;
                        return;
                    }
                }
                self.start_peer(hs.transport, conn, hs.remote_public, hs.handshake_hash)
                    .await;
            }
            Err(e) => {
                let _ = self
                    .events
                    .send(AppEvent::Error(format!("outbound handshake failed: {e}")))
                    .await;
            }
        }
    }

    /// Send a text message to a specific connected peer (by fingerprint).
    /// Returns the message id used to correlate its delivery acknowledgement.
    pub async fn send_to(&self, fingerprint: &str, text: String) -> Result<u64> {
        let id = self.next_msg_id.fetch_add(1, Ordering::Relaxed);
        let peers = self.peers.lock().await;
        let peer = peers
            .get(fingerprint)
            .ok_or_else(|| anyhow::anyhow!("peer not connected: {fingerprint}"))?;
        peer.outbound
            .send(Protocol::Text {
                id,
                body: text,
                ts: now_ts(),
            })
            .await
            .map_err(|_| anyhow::anyhow!("peer channel closed"))?;
        Ok(id)
    }

    /// Send a text message to every connected peer. Returns the shared message id
    /// and the fingerprints it was queued to, so callers can track delivery.
    pub async fn broadcast(&self, text: String) -> (u64, Vec<String>) {
        let id = self.next_msg_id.fetch_add(1, Ordering::Relaxed);
        let peers = self.peers.lock().await;
        let mut targets = Vec::with_capacity(peers.len());
        for (fingerprint, peer) in peers.iter() {
            if peer
                .outbound
                .send(Protocol::Broadcast {
                    id,
                    body: text.clone(),
                    ts: now_ts(),
                })
                .await
                .is_ok()
            {
                targets.push(fingerprint.clone());
            }
        }
        (id, targets)
    }

    /// A snapshot of connected peers as `(fingerprint, display_name)` pairs.
    pub async fn peer_list(&self) -> Vec<(String, String)> {
        self.peers
            .lock()
            .await
            .iter()
            .map(|(fp, peer)| (fp.clone(), peer.display_name.clone()))
            .collect()
    }

    /// Whether we currently have a live session with this peer.
    pub async fn is_connected(&self, fingerprint: &str) -> bool {
        self.peers.lock().await.contains_key(fingerprint)
    }

    /// Queue a graceful `Bye` to a peer. The peer replies by closing the
    /// connection, which lets a short-lived client (e.g. `whispr send`) confirm
    /// its message was delivered before exiting — avoiding the RST-on-abrupt-exit
    /// race that could otherwise drop the last frame.
    pub async fn disconnect(&self, fingerprint: &str) {
        let peers = self.peers.lock().await;
        if let Some(peer) = peers.get(fingerprint) {
            let _ = peer.outbound.send(Protocol::Bye).await;
        }
    }

    /// A clone of the app-event sender, for surfacing connection errors from
    /// background reconnect tasks.
    pub fn events_sender(&self) -> mpsc::Sender<AppEvent> {
        self.events.clone()
    }

    /// Register a freshly-handshaken connection and spawn its I/O task.
    async fn start_peer(
        &self,
        transport: TransportState,
        conn: RawConn,
        remote_public: Vec<u8>,
        handshake_hash: Vec<u8>,
    ) {
        let fingerprint = fingerprint_of(&remote_public);
        let session_id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let (out_tx, out_rx) = mpsc::channel::<Protocol>(64);

        {
            let mut peers = self.peers.lock().await;
            if let Some(existing) = peers.get(&fingerprint) {
                // Both sides dialed each other at the same moment. Each peer sees
                // the same two handshake hashes, so "keep the smaller one" makes
                // both ends keep the SAME connection (otherwise each would keep a
                // different one and close the other's, dropping the chat).
                if existing.established.elapsed() < DUPLICATE_WINDOW
                    && existing.handshake_hash < handshake_hash
                {
                    return; // drop this duplicate; `conn` closes here
                }
            }
            // Replacing an existing entry drops the old sender, which makes the
            // previous task's channel close and stops it promptly.
            peers.insert(
                fingerprint.clone(),
                ConnectedPeer {
                    session_id,
                    display_name: fingerprint.clone(),
                    outbound: out_tx.clone(),
                    handshake_hash,
                    established: Instant::now(),
                },
            );
        }
        let (reader, writer) = tokio::io::split(conn);

        let _ = self
            .events
            .send(AppEvent::PeerConnected {
                fingerprint: fingerprint.clone(),
                display_name: fingerprint.clone(),
                public_key_b58: bs58::encode(&remote_public).into_string(),
            })
            .await;

        // Introduce ourselves so the peer can display our name (also re-sent on
        // every reconnect, which fixes stale/placeholder names).
        let _ = out_tx
            .send(Protocol::Hello {
                display_name: self.identity.display_name.clone(),
                public_key_b58: self.identity.public_b58(),
            })
            .await;

        tokio::spawn(peer_task(
            session_id,
            transport,
            reader,
            writer,
            out_rx,
            fingerprint,
            self.events.clone(),
            self.peers.clone(),
        ));
    }
}

/// The per-connection task: multiplexes encrypted socket reads, plaintext
/// outbound messages, and a keepalive heartbeat over one Noise session.
#[allow(clippy::too_many_arguments)]
async fn peer_task(
    session_id: u64,
    mut cipher: TransportState,
    mut reader: ReadHalf<RawConn>,
    mut writer: WriteHalf<RawConn>,
    mut outbound: mpsc::Receiver<Protocol>,
    fingerprint: String,
    events: mpsc::Sender<AppEvent>,
    peers: Arc<Mutex<HashMap<String, ConnectedPeer>>>,
) {
    let mut in_buf = vec![0u8; u16::MAX as usize];
    let mut out_buf = vec![0u8; u16::MAX as usize];
    let established = Instant::now();
    // Set when either side said Bye: an intentional close, never a duplicate.
    let mut graceful = false;
    let mut last_seen = Instant::now();

    // Read frames in a dedicated task. `read_frame` is NOT cancel-safe: if it
    // sat directly in the `select!` below, a heartbeat tick or an outgoing
    // message firing mid-frame would discard the bytes already read. The stream
    // would then be misaligned, decryption would fail and the session would
    // drop. Receiving from a channel, by contrast, is cancel-safe.
    let (frame_tx, mut frames) = mpsc::channel::<Vec<u8>>(64);
    let reader_task = tokio::spawn(async move {
        while let Ok(frame) = read_frame(&mut reader).await {
            if frame_tx.send(frame).await.is_err() {
                break;
            }
        }
    });
    // Ids of messages we've sent that are still awaiting an ack, with the time
    // they were sent (for the delivery-timeout sweep).
    let mut pending_acks: HashMap<u64, Instant> = HashMap::new();
    let mut heartbeat = interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut ack_sweep = interval(ACK_SWEEP);
    ack_sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Encrypted frame arrived from the peer.
            frame = frames.recv() => {
                let Some(ciphertext) = frame else { break };
                last_seen = Instant::now();
                let Ok(len) = cipher.read_message(&ciphertext, &mut in_buf) else { break };
                let Ok(proto) = serde_json::from_slice::<Protocol>(&in_buf[..len]) else {
                    continue;
                };
                match proto {
                    Protocol::Text { id, body, ts } => {
                        let _ = events
                            .send(AppEvent::Message { from: fingerprint.clone(), text: body, ts, broadcast: false })
                            .await;
                        if send_ack(&mut cipher, &mut writer, &mut out_buf, id).await.is_err() {
                            break;
                        }
                    }
                    Protocol::Broadcast { id, body, ts } => {
                        let _ = events
                            .send(AppEvent::Message { from: fingerprint.clone(), text: body, ts, broadcast: true })
                            .await;
                        if send_ack(&mut cipher, &mut writer, &mut out_buf, id).await.is_err() {
                            break;
                        }
                    }
                    Protocol::Ack { id } => {
                        if pending_acks.remove(&id).is_some() {
                            let _ = events
                                .send(AppEvent::Delivered { peer: fingerprint.clone(), id })
                                .await;
                        }
                    }
                    Protocol::Hello { display_name, .. } => {
                        if let Some(peer) = peers.lock().await.get_mut(&fingerprint) {
                            peer.display_name = display_name.clone();
                        }
                        let _ = events
                            .send(AppEvent::PeerUpdated {
                                fingerprint: fingerprint.clone(),
                                display_name,
                            })
                            .await;
                    }
                    Protocol::Ping => {
                        // Reply so the peer knows we're alive.
                        if let Ok(pt) = serde_json::to_vec(&Protocol::Pong) {
                            if let Ok(n) = cipher.write_message(&pt, &mut out_buf) {
                                if write_frame(&mut writer, &out_buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Protocol::Pong => {} // liveness only; last_seen already refreshed
                    Protocol::Bye => {
                        graceful = true;
                        break;
                    }
                }
            }

            // Outbound message from the app to encrypt and send.
            maybe = outbound.recv() => {
                let Some(proto) = maybe else { break };
                // Remember chat messages so we can confirm (or time out) delivery.
                if matches!(proto, Protocol::Bye) {
                    graceful = true;
                }
                let track = match &proto {
                    Protocol::Text { id, .. } | Protocol::Broadcast { id, .. } => Some(*id),
                    _ => None,
                };
                let Ok(plaintext) = serde_json::to_vec(&proto) else { continue };
                let Ok(len) = cipher.write_message(&plaintext, &mut out_buf) else { break };
                if write_frame(&mut writer, &out_buf[..len]).await.is_err() {
                    break;
                }
                if let Some(id) = track {
                    pending_acks.insert(id, Instant::now());
                }
            }

            // Report messages that have gone unacknowledged for too long.
            _ = ack_sweep.tick() => {
                let now = Instant::now();
                let overdue: Vec<u64> = pending_acks
                    .iter()
                    .filter(|(_, sent)| now.duration_since(**sent) > DELIVERY_TIMEOUT)
                    .map(|(id, _)| *id)
                    .collect();
                for id in overdue {
                    pending_acks.remove(&id);
                    let _ = events
                        .send(AppEvent::DeliveryFailed { peer: fingerprint.clone(), id })
                        .await;
                }
            }

            // Keepalive tick: give up if the peer has gone silent, else ping.
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > PEER_TIMEOUT {
                    break;
                }
                let Ok(plaintext) = serde_json::to_vec(&Protocol::Ping) else { continue };
                let Ok(len) = cipher.write_message(&plaintext, &mut out_buf) else { break };
                if write_frame(&mut writer, &out_buf[..len]).await.is_err() {
                    break;
                }
            }
        }
    }

    reader_task.abort();
    drop(writer);

    // Any still-unacknowledged messages will never be confirmed on this
    // connection; report them so the UI can flag them as undelivered (even if a
    // newer session replaced us, otherwise they'd stay "pending" forever).
    for id in pending_acks.into_keys() {
        let _ = events
            .send(AppEvent::DeliveryFailed { peer: fingerprint.clone(), id })
            .await;
    }

    // A young session closing is usually the losing half of a simultaneous
    // dial; give the winning session a moment to register before deciding the
    // peer is actually gone.
    if !graceful && established.elapsed() < DUPLICATE_WINDOW {
        tokio::time::sleep(DUPLICATE_GRACE).await;
    }

    // Only tear down state if we're still the current session for this peer; a
    // newer reconnect may have already replaced us.
    let mut peers = peers.lock().await;
    if peers.get(&fingerprint).map(|p| p.session_id) == Some(session_id) {
        peers.remove(&fingerprint);
        drop(peers);
        let _ = events
            .send(AppEvent::PeerDisconnected { fingerprint })
            .await;
    }
}

/// Read one `u16` length-prefixed frame of ciphertext.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let len = u16::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write one `u16` length-prefixed frame of ciphertext.
async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    let len = u16::try_from(data.len()).map_err(|_| anyhow::anyhow!("frame too large"))?;
    // One write per frame: header and body leave together in one segment.
    let mut frame = Vec::with_capacity(2 + data.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(data);
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// Encrypt and send an [`Protocol::Ack`] for `id`. Errors mirror a broken
/// connection so the caller can tear the session down.
async fn send_ack(
    cipher: &mut TransportState,
    writer: &mut WriteHalf<RawConn>,
    out_buf: &mut [u8],
    id: u64,
) -> Result<()> {
    let plaintext = serde_json::to_vec(&Protocol::Ack { id })?;
    let len = cipher
        .write_message(&plaintext, out_buf)
        .map_err(|e| anyhow::anyhow!("encrypt ack: {e}"))?;
    write_frame(writer, &out_buf[..len]).await
}
