//! LAN transport — the fully-working reference implementation.
//!
//! Peers on the same Wi-Fi/Ethernet segment find each other automatically over
//! multicast DNS (mDNS / DNS-SD), then talk directly over TCP. No internet, no
//! server and no configuration required — this is the "offline" mode.

use std::sync::Arc;

use async_trait::async_trait;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{DiscoveredPeer, RawConn, Transport, TransportEvent};
use crate::identity::Identity;

/// The DNS-SD service type whispr peers advertise themselves under.
const SERVICE_TYPE: &str = "_whispr._tcp.local.";

/// How long a LAN dial may take before we give up (peers are local, so a stalled
/// dial almost always means the peer just vanished).
const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct LanTransport {
    port: u16,
    identity: Arc<Identity>,
}

impl LanTransport {
    pub fn new(port: u16, identity: Arc<Identity>) -> anyhow::Result<Self> {
        Ok(Self { port, identity })
    }
}

#[async_trait]
impl Transport for LanTransport {
    fn name(&self) -> &'static str {
        "lan"
    }

    fn supports_discovery(&self) -> bool {
        true
    }

    async fn serve(&self, events: mpsc::Sender<TransportEvent>) -> anyhow::Result<()> {
        // Bind the TCP listener first so we can advertise the real port.
        let listener = TcpListener::bind(("0.0.0.0", self.port)).await?;
        let port = listener.local_addr()?.port();

        // Advertise ourselves and start browsing for others over mDNS.
        let mdns = ServiceDaemon::new()?;
        // NOTE: mdns-sd mishandles uppercase letters in the host label, so the
        // instance/host name must be lowercase. Identity is carried in the `pk`
        // TXT record, which may contain any case.
        let instance = self.identity.fingerprint().to_lowercase();
        let host_name = format!("{instance}.local.");
        let public_b58 = self.identity.public_b58();
        let display_name = self.identity.display_name.clone();
        let properties = [
            ("pk", public_b58.as_str()),
            ("name", display_name.as_str()),
        ];
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host_name,
            "",
            port,
            &properties[..],
        )?
        .enable_addr_auto();
        mdns.register(service)?;

        let browse = mdns.browse(SERVICE_TYPE)?;
        let my_public_b58 = public_b58.clone();
        let disc_tx = events.clone();
        tokio::spawn(async move {
            while let Ok(event) = browse.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let public_key_b58 =
                            info.get_property_val_str("pk").map(|s| s.to_string());
                        // Ignore our own advertisement (compare identity, not name).
                        if public_key_b58.as_deref() == Some(my_public_b58.as_str()) {
                            continue;
                        }
                        // Prefer a dialable IPv4 address.
                        let Some(ip) = info.get_addresses().iter().find(|a| a.is_ipv4()) else {
                            continue;
                        };
                        let addr = format!("{ip}:{}", info.get_port());
                        let display_name = info
                            .get_property_val_str("name")
                            .unwrap_or("anon")
                            .to_string();
                        let peer = DiscoveredPeer {
                            id: info.get_fullname().to_string(),
                            display_name,
                            addr,
                            public_key_b58,
                        };
                        if disc_tx.send(TransportEvent::Discovered(peer)).await.is_err() {
                            break;
                        }
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        if disc_tx.send(TransportEvent::Lost(fullname)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        // Accept inbound TCP connections forever.
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
        let stream = timeout(DIAL_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow::anyhow!("dial to {addr} timed out"))??;
        let _ = stream.set_nodelay(true);
        Ok(Box::new(stream))
    }
}
