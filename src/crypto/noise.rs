//! The Noise-protocol handshake that gives every whispr conversation
//! end-to-end encryption, forward secrecy and mutual authentication.
//!
//! We use the `Noise_XX_25519_ChaChaPoly_BLAKE2s` pattern:
//!
//! - **XX**  — both sides transmit their static public key *during* the
//!   handshake, so neither needs to know the other in advance and each learns
//!   (and can pin) the other's identity key.
//! - **25519** — X25519 Diffie-Hellman.
//! - **ChaChaPoly** — ChaCha20-Poly1305 AEAD for the transport.
//! - **BLAKE2s** — the hash used to mix handshake state.
//!
//! The handshake runs over any bidirectional byte stream (TCP today, BLE GATT
//! later) using a simple `u16` length-prefixed framing.

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use snow::params::NoiseParams;
use snow::{Builder, TransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

/// The Noise handshake pattern and cipher suite used throughout whispr.
pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Maximum wall-clock time a handshake may take before it is aborted. This stops
/// a peer that opens a connection but never (or only slowly) completes the
/// handshake from pinning a task, socket and buffers indefinitely — a
/// slowloris-style denial of service against inbound accepts and outbound dials.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A static X25519 key pair.
pub struct Keypair {
    pub public: Vec<u8>,
    pub private: Vec<u8>,
}

fn params() -> Result<NoiseParams> {
    NOISE_PARAMS
        .parse()
        .map_err(|e| anyhow!("invalid noise params: {e:?}"))
}

/// Generate a fresh static X25519 key pair.
pub fn generate_keypair() -> Result<Keypair> {
    let builder = Builder::new(params()?);
    let kp = builder.generate_keypair()?;
    Ok(Keypair {
        public: kp.public,
        private: kp.private,
    })
}

/// The outcome of a completed handshake: an established cipher session plus the
/// verified static public key of the remote peer.
pub struct Handshake {
    pub transport: TransportState,
    pub remote_public: Vec<u8>,
    /// The Noise handshake hash: identical on both ends and unique per session,
    /// so both peers can agree on which of two simultaneous connections to keep.
    pub handshake_hash: Vec<u8>,
}

/// Perform the Noise XX handshake as the **initiator** (the side that dialed).
pub async fn handshake_initiator<S>(stream: &mut S, local_private: &[u8]) -> Result<Handshake>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(HANDSHAKE_TIMEOUT, async {
        let mut hs = Builder::new(params()?)
            .local_private_key(local_private)
            .build_initiator()?;
        let mut buf = vec![0u8; u16::MAX as usize];

        // -> e
        let n = hs.write_message(&[], &mut buf)?;
        write_msg(stream, &buf[..n]).await?;

        // <- e, ee, s, es
        let msg = read_msg(stream).await?;
        hs.read_message(&msg, &mut buf)?;

        // -> s, se
        let n = hs.write_message(&[], &mut buf)?;
        write_msg(stream, &buf[..n]).await?;

        finish(hs)
    })
    .await
    .map_err(|_| anyhow!("handshake timed out"))?
}

/// Perform the Noise XX handshake as the **responder** (the side that accepted).
pub async fn handshake_responder<S>(stream: &mut S, local_private: &[u8]) -> Result<Handshake>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(HANDSHAKE_TIMEOUT, async {
        let mut hs = Builder::new(params()?)
            .local_private_key(local_private)
            .build_responder()?;
        let mut buf = vec![0u8; u16::MAX as usize];

        // <- e
        let msg = read_msg(stream).await?;
        hs.read_message(&msg, &mut buf)?;

        // -> e, ee, s, es
        let n = hs.write_message(&[], &mut buf)?;
        write_msg(stream, &buf[..n]).await?;

        // <- s, se
        let msg = read_msg(stream).await?;
        hs.read_message(&msg, &mut buf)?;

        finish(hs)
    })
    .await
    .map_err(|_| anyhow!("handshake timed out"))?
}

fn finish(hs: snow::HandshakeState) -> Result<Handshake> {
    let remote_public = hs
        .get_remote_static()
        .map(|k| k.to_vec())
        .ok_or_else(|| anyhow!("peer did not present a static key"))?;
    let handshake_hash = hs.get_handshake_hash().to_vec();
    let transport = hs.into_transport_mode()?;
    Ok(Handshake {
        transport,
        remote_public,
        handshake_hash,
    })
}

/// Serialize `msg`, seal it with the Noise transport cipher, and write it as a
/// single length-prefixed frame. Used for the request/response directory
/// protocol, where each side alternates writing and reading.
pub async fn write_encrypted<S, T>(
    stream: &mut S,
    cipher: &mut TransportState,
    msg: &T,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let plaintext = serde_json::to_vec(msg)?;
    if plaintext.len() > (u16::MAX as usize - 16) {
        return Err(anyhow!("message too large for a single frame"));
    }
    let mut buf = vec![0u8; plaintext.len() + 64];
    let n = cipher.write_message(&plaintext, &mut buf)?;
    write_msg(stream, &buf[..n]).await
}

/// Read one length-prefixed frame, decrypt it with the Noise transport cipher,
/// and deserialize the contained value.
pub async fn read_encrypted<S, T>(stream: &mut S, cipher: &mut TransportState) -> Result<T>
where
    S: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let frame = read_msg(stream).await?;
    let mut buf = vec![0u8; frame.len()];
    let n = cipher.read_message(&frame, &mut buf)?;
    Ok(serde_json::from_slice(&buf[..n])?)
}

/// Write a single `u16` length-prefixed frame.
async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    let len = u16::try_from(data.len()).map_err(|_| anyhow!("handshake frame too large"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(data).await?;
    w.flush().await?;
    Ok(())
}

/// Read a single `u16` length-prefixed frame.
async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let len = u16::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}
