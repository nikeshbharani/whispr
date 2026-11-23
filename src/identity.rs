//! Anonymous, self-sovereign identities.
//!
//! An identity is nothing more than a static X25519 key pair. There is no phone
//! number, email or account behind it — your *public key* is your address and a
//! short hash of it is your human-readable *fingerprint*. This gives you
//! anonymity (no personally identifying information) while still letting peers
//! cryptographically verify who they are talking to.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::identity_path;

/// A long-term identity: a static key pair plus a chosen display name.
#[derive(Debug, Clone)]
pub struct Identity {
    /// 32-byte X25519 private key. Never leaves this process.
    pub private: Vec<u8>,
    /// 32-byte X25519 public key. Doubles as the peer's network address/ID.
    pub public: Vec<u8>,
    /// A freely chosen, non-unique display name (defaults to an anonymous handle).
    pub display_name: String,
}

/// On-disk representation of an [`Identity`].
#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    private_b58: String,
    public_b58: String,
    display_name: String,
}

impl Identity {
    /// Generate a brand-new random identity.
    pub fn generate(display_name: impl Into<String>) -> Result<Self> {
        let kp = crate::crypto::noise::generate_keypair()?;
        Ok(Self {
            private: kp.private,
            public: kp.public,
            display_name: display_name.into(),
        })
    }

    /// Load the identity from disk, creating (and persisting) a new one if none
    /// exists yet. An explicit `display_name` overrides the stored one.
    pub fn load_or_create(display_name: Option<String>) -> Result<Self> {
        let path = identity_path();
        if path.exists() {
            let mut identity = Self::load(&path)?;
            if let Some(name) = display_name {
                identity.display_name = name;
            }
            Ok(identity)
        } else {
            let identity = Self::generate(display_name.unwrap_or_else(random_name))?;
            identity.save(&path)?;
            Ok(identity)
        }
    }

    /// Load an identity from a specific file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading identity file {}", path.display()))?;
        let stored: StoredIdentity =
            serde_json::from_str(&raw).context("parsing identity file")?;
        let private = bs58::decode(&stored.private_b58)
            .into_vec()
            .context("decoding private key")?;
        let public = bs58::decode(&stored.public_b58)
            .into_vec()
            .context("decoding public key")?;
        Ok(Self {
            private,
            public,
            display_name: stored.display_name,
        })
    }

    /// Persist this identity to a file, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating data directory {}", parent.display()))?;
        }
        let stored = StoredIdentity {
            private_b58: bs58::encode(&self.private).into_string(),
            public_b58: bs58::encode(&self.public).into_string(),
            display_name: self.display_name.clone(),
        };
        let json = serde_json::to_string_pretty(&stored)?;
        fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
        // Best-effort tighten permissions on Unix — the private key lives here.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// The full public key, base58-encoded. This is your dialable network ID.
    pub fn public_b58(&self) -> String {
        bs58::encode(&self.public).into_string()
    }

    /// A short, human-comparable fingerprint derived from the public key.
    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.public)
    }
}

/// Compute the short fingerprint for an arbitrary public key.
pub fn fingerprint_of(public_key: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    let digest = hasher.finalize();
    bs58::encode(&digest[..16]).into_string()
}

/// Generate a throwaway anonymous handle such as `anon-3f9c`.
pub fn random_name() -> String {
    let mut rng = rand::thread_rng();
    let suffix: u16 = rng.r#gen();
    format!("anon-{suffix:04x}")
}
