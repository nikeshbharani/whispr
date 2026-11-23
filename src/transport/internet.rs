//! Internet transport — direct TCP P2P, optionally routed for anonymity.
//!
//! There is still no server: you accept inbound connections on a port and dial
//! peers by address. What makes it *anonymous* is the [`NetworkMode`]: dialing
//! can be tunnelled through Tor's SOCKS5 proxy (or any SOCKS5 proxy), which
//! hides your IP from the peer and lets you reach `.onion` addresses. The
//! end-to-end encryption from the Noise layer is always applied on top.

use async_trait::async_trait;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_socks::tcp::Socks5Stream;

use super::{RawConn, Transport, TransportEvent};
use crate::config::NetworkMode;

/// How long a direct TCP dial may take before we give up. Without this a
/// routable-but-silent host would hang the dial for the OS default (~75s),
/// leaking a pending connection.
const DIRECT_DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Proxied dials (Tor/onion) are legitimately slower to establish, so they get
/// a more generous budget.
const PROXY_DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct InternetTransport {
    port: u16,
    net: NetworkMode,
}

impl InternetTransport {
    pub fn new(port: u16, net: NetworkMode) -> anyhow::Result<Self> {
        Ok(Self { port, net })
    }
}

#[async_trait]
impl Transport for InternetTransport {
    fn name(&self) -> &'static str {
        "internet"
    }

    async fn serve(&self, events: mpsc::Sender<TransportEvent>) -> anyhow::Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", self.port)).await?;
        loop {
            let (stream, from) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            if events
                .send(TransportEvent::Inbound {
                    conn: Box::new(stream),
                    from: Some(from),
                })
                .await
                .is_err()
            {
                break;
            }
        }
        Ok(())
    }

    async fn dial(&self, addr: &str) -> anyhow::Result<RawConn> {
        match self.net.proxy_addr() {
            Some(proxy) => {
                // Tunnel the connection through the SOCKS5 proxy (e.g. Tor).
                let stream = timeout(PROXY_DIAL_TIMEOUT, Socks5Stream::connect(proxy.as_str(), addr))
                    .await
                    .map_err(|_| anyhow::anyhow!("dial to {addr} via proxy timed out"))??;
                Ok(Box::new(stream.into_inner()))
            }
            None => {
                let stream = timeout(DIRECT_DIAL_TIMEOUT, TcpStream::connect(addr))
                    .await
                    .map_err(|_| anyhow::anyhow!("dial to {addr} timed out"))??;
                let _ = stream.set_nodelay(true);
                Ok(Box::new(stream))
            }
        }
    }
}
