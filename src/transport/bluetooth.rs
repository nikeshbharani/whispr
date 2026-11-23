//! Bluetooth Low Energy transport — experimental scaffold.
//!
//! This wires BLE into the same [`Transport`] abstraction as the other modes so
//! the rest of the app needs no changes. Peer *discovery* (scanning for nearby
//! whispr devices) is implemented; the encrypted GATT data channel that would
//! carry the Noise handshake is stubbed out and returns a clear error.
//!
//! It is compiled only when the `bluetooth` feature is enabled, because it pulls
//! in a native BLE stack:
//!
//! ```text
//! cargo run --features bluetooth -- --mode bluetooth chat
//! ```

use std::sync::Arc;

use super::Transport;
use crate::identity::Identity;

/// Build the Bluetooth transport (only available with the `bluetooth` feature).
#[cfg(feature = "bluetooth")]
pub fn build(identity: Arc<Identity>) -> anyhow::Result<Arc<dyn Transport>> {
    Ok(Arc::new(ble::BleTransport::new(identity)?))
}

/// Fallback used when the crate is built without the `bluetooth` feature.
#[cfg(not(feature = "bluetooth"))]
pub fn build(_identity: Arc<Identity>) -> anyhow::Result<Arc<dyn Transport>> {
    anyhow::bail!(
        "Bluetooth chat isn't available yet. BLE support is experimental and messaging \
         over Bluetooth is not implemented (the cross-platform BLE library can scan for \
         devices but cannot act as a peer to exchange data). Please use Wi-Fi/LAN or \
         Internet mode to chat."
    )
}

#[cfg(feature = "bluetooth")]
mod ble {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
    use btleplug::platform::Manager;
    use tokio::sync::mpsc;

    use crate::identity::Identity;
    use crate::transport::{DiscoveredPeer, RawConn, Transport, TransportEvent};

    pub struct BleTransport {
        _identity: Arc<Identity>,
    }

    impl BleTransport {
        pub fn new(identity: Arc<Identity>) -> anyhow::Result<Self> {
            Ok(Self {
                _identity: identity,
            })
        }
    }

    #[async_trait]
    impl Transport for BleTransport {
        fn name(&self) -> &'static str {
            "bluetooth"
        }

        fn supports_discovery(&self) -> bool {
            true
        }

        async fn serve(&self, events: mpsc::Sender<TransportEvent>) -> anyhow::Result<()> {
            let manager = Manager::new().await?;
            let adapter = manager
                .adapters()
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("no Bluetooth adapter found"))?;

            adapter.start_scan(ScanFilter::default()).await?;

            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                for peripheral in adapter.peripherals().await? {
                    if let Ok(Some(props)) = peripheral.properties().await {
                        let id = props.address.to_string();
                        let display_name =
                            props.local_name.unwrap_or_else(|| "ble-peer".to_string());
                        let peer = DiscoveredPeer {
                            id: id.clone(),
                            display_name,
                            addr: id,
                            public_key_b58: None,
                        };
                        if events.send(TransportEvent::Discovered(peer)).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        }

        async fn dial(&self, _addr: &str) -> anyhow::Result<RawConn> {
            anyhow::bail!(
                "Bluetooth discovery works, but messaging over BLE is not implemented yet: \
                 the cross-platform BLE library can scan/connect as a central but cannot act \
                 as the GATT peripheral that two peers need to exchange data. Use LAN or \
                 Internet mode to chat for now."
            )
        }
    }
}
