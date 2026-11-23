//! Runtime configuration: which transport to use, the network-privacy policy and
//! where persistent state (your identity) lives.

use std::path::PathBuf;

use directories::ProjectDirs;

/// Which physical/logical transport carries the encrypted messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    /// Local network only. No internet required. Peers auto-discover over mDNS.
    Lan,
    /// Peer-to-peer over the internet, optionally through Tor or a SOCKS5 proxy.
    Internet,
    /// Bluetooth Low Energy. Fully offline, short range (experimental scaffold).
    Bluetooth,
}

impl TransportMode {
    /// A short, human-friendly description for status bars and CLI output.
    pub fn label(&self) -> &'static str {
        match self {
            TransportMode::Lan => "LAN (local network, offline)",
            TransportMode::Internet => "Internet (P2P)",
            TransportMode::Bluetooth => "Bluetooth (BLE)",
        }
    }
}

/// Network-level privacy policy applied when dialing peers over the internet.
///
/// End-to-end encryption is always on regardless of this setting; this only
/// controls how (and how anonymously) the underlying connection is routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMode {
    /// Connect directly. Fastest, but exposes your IP address to the peer.
    Direct,
    /// Route through a local Tor SOCKS5 proxy (127.0.0.1:9050) to hide your IP.
    Tor,
    /// Route through a custom SOCKS5 proxy given as `host:port`.
    Proxy(String),
}

impl NetworkMode {
    /// The SOCKS5 proxy address to dial through, if any.
    pub fn proxy_addr(&self) -> Option<String> {
        match self {
            NetworkMode::Direct => None,
            NetworkMode::Tor => Some("127.0.0.1:9050".to_string()),
            NetworkMode::Proxy(addr) => Some(addr.clone()),
        }
    }

    /// A short, human-friendly description for status bars and CLI output.
    pub fn label(&self) -> String {
        match self {
            NetworkMode::Direct => "direct".to_string(),
            NetworkMode::Tor => "tor".to_string(),
            NetworkMode::Proxy(addr) => format!("proxy({addr})"),
        }
    }
}

/// The directory where persistent state (identity key) is stored.
pub fn data_dir() -> PathBuf {
    ProjectDirs::from("dev", "whispr", "whispr")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".whispr"))
}

/// The path of the long-term identity key file.
pub fn identity_path() -> PathBuf {
    data_dir().join("identity.json")
}
