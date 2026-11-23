//! whispr — a serverless, end-to-end encrypted, anonymous peer-to-peer messenger.
//!
//! The crate is organised as a set of small, single-responsibility modules so
//! that transports, cryptography and the user interfaces can evolve
//! independently:
//!
//! - [`identity`]   — anonymous, self-sovereign identities (static X25519 keys).
//! - [`crypto`]     — the Noise-protocol handshake that provides E2E encryption.
//! - [`message`]    — the wire protocol exchanged between peers.
//! - [`transport`]  — pluggable transports (LAN, Internet+Tor/proxy, Bluetooth).
//! - [`session`]    — connection/peer management on top of any transport.
//! - [`app`]        — the orchestration layer the UIs drive.
//! - [`ui`]         — the CLI and the interactive TUI.

pub mod app;
pub mod config;
pub mod crypto;
pub mod directory;
pub mod identity;
pub mod message;
pub mod session;
pub mod transport;
pub mod ui;
