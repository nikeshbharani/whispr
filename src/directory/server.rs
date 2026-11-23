//! The directory server: accepts encrypted, Noise-authenticated connections and
//! maintains an in-memory `public key -> address` table with per-record TTLs.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::{DirRequest, DirResponse};
use crate::crypto::noise;
use crate::identity::fingerprint_of;

/// How long a registration stays live without being refreshed.
const RECORD_TTL: Duration = Duration::from_secs(300);

/// Drop a client that completes its handshake but then sits idle without sending
/// a request, so it cannot hold a connection (and task) open indefinitely.
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Upper bound on the number of live records. Prevents an attacker from
/// exhausting server memory by registering an unbounded number of keys.
const MAX_RECORDS: usize = 100_000;

struct Record {
    address: String,
    expires: Instant,
}

#[derive(Clone, Default)]
struct Directory {
    entries: Arc<Mutex<HashMap<Vec<u8>, Record>>>,
}

/// Run the directory server on `bind` (e.g. `0.0.0.0:6060`) until interrupted.
pub async fn run(bind: &str) -> Result<()> {
    // Each run uses an ephemeral server identity. Clients may pin its key.
    let server_kp = noise::generate_keypair()?;

    let listener = TcpListener::bind(bind).await?;
    println!("whispr directory server");
    println!("  listening on : {}", listener.local_addr()?);
    println!(
        "  server key   : {}",
        bs58::encode(&server_kp.public).into_string()
    );
    println!("  fingerprint  : {}", fingerprint_of(&server_kp.public));
    println!("  record TTL   : {}s", RECORD_TTL.as_secs());

    let directory = Directory::default();
    loop {
        let (stream, peer) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let directory = directory.clone();
        let server_priv = server_kp.private.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, peer, directory, server_priv).await {
                let _ = e; // client handshake/connection errors are non-fatal
            }
        });
    }
}

async fn handle_client(
    mut stream: TcpStream,
    peer: SocketAddr,
    directory: Directory,
    server_priv: Vec<u8>,
) -> Result<()> {
    // The handshake proves the client owns the key it presents.
    let hs = noise::handshake_responder(&mut stream, &server_priv).await?;
    let mut cipher = hs.transport;
    let client_key = hs.remote_public;

    loop {
        let request: DirRequest =
            match timeout(CLIENT_IDLE_TIMEOUT, noise::read_encrypted(&mut stream, &mut cipher)).await
            {
                Ok(Ok(request)) => request,
                // Idle too long, or the client disconnected / sent garbage.
                _ => break,
            };

        let response = match request {
            DirRequest::Register { port } => {
                let address = format!("{}:{}", peer.ip(), port);
                let mut entries = directory.entries.lock().await;
                let now = Instant::now();
                // Opportunistically evict expired records so the cap reflects
                // only live registrations.
                entries.retain(|_, r| r.expires > now);
                if !entries.contains_key(&client_key) && entries.len() >= MAX_RECORDS {
                    DirResponse::Error("directory is full, try again later".to_string())
                } else {
                    entries.insert(
                        client_key.clone(),
                        Record {
                            address: address.clone(),
                            expires: now + RECORD_TTL,
                        },
                    );
                    DirResponse::Registered { address }
                }
            }
            DirRequest::Lookup { public_key_b58 } => match bs58::decode(&public_key_b58).into_vec()
            {
                Ok(key) => {
                    let mut entries = directory.entries.lock().await;
                    match entries.get(&key) {
                        Some(record) if record.expires > Instant::now() => DirResponse::Found {
                            address: record.address.clone(),
                        },
                        Some(_) => {
                            entries.remove(&key);
                            DirResponse::NotFound
                        }
                        None => DirResponse::NotFound,
                    }
                }
                Err(_) => DirResponse::Error("invalid public key".to_string()),
            },
            DirRequest::Ping => DirResponse::Pong,
        };

        noise::write_encrypted(&mut stream, &mut cipher, &response).await?;
    }
    Ok(())
}
