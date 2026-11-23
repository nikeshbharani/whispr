//! User interfaces.
//!
//! - [`cli`] — the `clap`-based command surface (identity management, scriptable
//!   one-shot send/listen/discover, and launching the TUI).
//! - [`tui`] — a full-screen, chat-style interactive terminal UI.

pub mod cli;
pub mod tui;
