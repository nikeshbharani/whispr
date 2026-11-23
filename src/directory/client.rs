//! The client side of the directory protocol: register our own address and look
//! up peers by public key. Each call opens a short-lived encrypted connection.

use anyhow::{anyhow, Result};
use tokio::net::TcpStream;

use super::{DirRequest, DirResponse};
use crate::crypto::noise;

/// A handle to a directory server. Cheap to clone.
#[derive(Clone)]
pub struct DirectoryClient {
    server_addr: String,
}

impl DirectoryClient {
    pub fn new(server_addr: impl Into<String>) -> Self {
        Self {
            server_addr: server_addr.into(),
        }
    }

    /// Register our identity so peers can find us. We authenticate with our real
    /// identity key (proving ownership); the server records our observed IP with
    /// the `listen_port` we advertise. Returns the address the server recorded.
    pub async fn register(&self, identity_private: &[u8], listen_port: u16) -> Result<String> {
        let mut stream = TcpStream::connect(&self.server_addr).await?;
        let hs = noise::handshake_initiator(&mut stream, identity_private).await?;
        let mut cipher = hs.transport;

        noise::write_encrypted(
            &mut stream,
            &mut cipher,
            &DirRequest::Register { port: listen_port },
        )
        .await?;

        match noise::read_encrypted::<_, DirResponse>(&mut stream, &mut cipher).await? {
            DirResponse::Registered { address } => Ok(address),
            DirResponse::Error(e) => Err(anyhow!("directory error: {e}")),
            _ => Err(anyhow!("unexpected directory response to register")),
        }
    }

    /// Look up a peer's current address by their public key. A throwaway key is
    /// used for the handshake so the lookup is not linked to our identity.
    pub async fn lookup(&self, public_key_b58: &str) -> Result<Option<String>> {
        let ephemeral = noise::generate_keypair()?;
        let mut stream = TcpStream::connect(&self.server_addr).await?;
        let hs = noise::handshake_initiator(&mut stream, &ephemeral.private).await?;
        let mut cipher = hs.transport;

        noise::write_encrypted(
            &mut stream,
            &mut cipher,
            &DirRequest::Lookup {
                public_key_b58: public_key_b58.to_string(),
            },
        )
        .await?;

        match noise::read_encrypted::<_, DirResponse>(&mut stream, &mut cipher).await? {
            DirResponse::Found { address } => Ok(Some(address)),
            DirResponse::NotFound => Ok(None),
            DirResponse::Error(e) => Err(anyhow!("directory error: {e}")),
            _ => Err(anyhow!("unexpected directory response to lookup")),
        }
    }
}
