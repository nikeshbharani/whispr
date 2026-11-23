//! The command-line surface.
//!
//! Running whispr with no subcommand launches the interactive TUI. The other
//! subcommands are scriptable, line-oriented tools for identity management and
//! headless messaging.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::app::{App, AppEvent};
use crate::config::{self, NetworkMode, TransportMode};
use crate::directory::{self, client::DirectoryClient};
use crate::identity::{self, Identity};

/// Serverless, end-to-end encrypted, anonymous peer-to-peer messenger.
#[derive(Parser)]
#[command(
    name = "whispr",
    version,
    about = "Serverless, E2E-encrypted, anonymous P2P messenger (LAN / Internet+Tor / Bluetooth).",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub common: CommonArgs,
}

/// Flags shared by every subcommand.
#[derive(Args, Clone)]
pub struct CommonArgs {
    /// Transport to use.
    #[arg(short, long, value_enum, default_value_t = ModeArg::Lan, global = true)]
    pub mode: ModeArg,

    /// Network privacy policy for the Internet transport (E2E is always on).
    #[arg(long, value_enum, default_value_t = NetArg::Direct, global = true)]
    pub net: NetArg,

    /// Custom SOCKS5 proxy `host:port` (used with `--net proxy`).
    #[arg(long, global = true)]
    pub proxy: Option<String>,

    /// Display name to advertise (an anonymous handle is used by default).
    #[arg(long, global = true)]
    pub name: Option<String>,

    /// TCP port to listen on (0 = pick a random free port).
    #[arg(short = 'p', long, default_value_t = 0, global = true)]
    pub port: u16,

    /// Rendezvous directory server `host:port` (Internet transport only).
    #[arg(long, global = true)]
    pub directory: Option<String>,
}

#[derive(Copy, Clone, ValueEnum)]
pub enum ModeArg {
    Lan,
    Internet,
    Bluetooth,
}

#[derive(Copy, Clone, ValueEnum)]
pub enum NetArg {
    Direct,
    Tor,
    Proxy,
}

impl CommonArgs {
    fn transport_mode(&self) -> TransportMode {
        match self.mode {
            ModeArg::Lan => TransportMode::Lan,
            ModeArg::Internet => TransportMode::Internet,
            ModeArg::Bluetooth => TransportMode::Bluetooth,
        }
    }

    fn network_mode(&self) -> Result<NetworkMode> {
        Ok(match self.net {
            NetArg::Direct => NetworkMode::Direct,
            NetArg::Tor => NetworkMode::Tor,
            NetArg::Proxy => NetworkMode::Proxy(
                self.proxy
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("--proxy host:port is required with --net proxy"))?,
            ),
        })
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Show your anonymous identity (fingerprint + public key).
    Id,
    /// Generate a fresh identity, replacing any existing one.
    Keygen {
        /// Overwrite an existing identity without asking.
        #[arg(long)]
        force: bool,
    },
    /// Launch the interactive chat UI (this is also the default).
    Chat {
        /// Optionally auto-connect to a peer address on startup.
        #[arg(long)]
        connect: Option<String>,
    },
    /// Discover peers on the network and exit (LAN/Bluetooth modes).
    Discover {
        /// How many seconds to scan for.
        #[arg(long, default_value_t = 5)]
        secs: u64,
    },
    /// Run a headless node: accept connections and print messages.
    Listen {
        /// Optionally dial and stay connected to a peer address on startup.
        #[arg(long)]
        connect: Option<String>,
    },
    /// Connect to a peer, send one message, and exit.
    Send {
        /// Peer address, e.g. `host:port`.
        addr: String,
        /// The message text to send.
        message: String,
    },
    /// Run a rendezvous directory server (for the Internet transport).
    Directory {
        /// Address to bind, e.g. `0.0.0.0:6060`.
        #[arg(long, default_value_t = format!("0.0.0.0:{}", directory::DEFAULT_PORT))]
        bind: String,
    },
    /// Look up a peer's current address in the directory by public key.
    Find {
        /// The peer's full base58 public key.
        public_key: String,
    },
    /// Look a peer up by key via the directory, send one message, and exit.
    Reach {
        /// The peer's full base58 public key.
        public_key: String,
        /// The message text to send.
        message: String,
    },
}

/// Entry point invoked from `main`.
pub async fn run(cli: Cli) -> Result<()> {
    let mode = cli.common.transport_mode();
    let net = cli.common.network_mode()?;
    let name = cli.common.name.clone();
    let port = cli.common.port;
    let directory = cli.common.directory.clone();

    match cli.command.unwrap_or(Command::Chat { connect: None }) {
        Command::Id => cmd_id(name)?,
        Command::Keygen { force } => cmd_keygen(name, force)?,
        Command::Discover { secs } => cmd_discover(mode, net, name, port, secs).await?,
        Command::Listen { connect } => cmd_listen(mode, net, name, port, directory, connect).await?,
        Command::Send { addr, message } => {
            cmd_send(mode, net, name, port, directory, addr, message).await?
        }
        Command::Directory { bind } => directory::server::run(&bind).await?,
        Command::Find { public_key } => cmd_find(directory, public_key).await?,
        Command::Reach { public_key, message } => {
            cmd_reach(mode, net, name, port, directory, public_key, message).await?
        }
        Command::Chat { connect } => {
            let identity = Identity::load_or_create(name)?;
            crate::ui::tui::run_tui(identity, mode, net, port, directory, connect).await?;
        }
    }
    Ok(())
}

fn cmd_id(name: Option<String>) -> Result<()> {
    let identity = Identity::load_or_create(name)?;
    println!("Display name : {}", identity.display_name);
    println!("Fingerprint  : {}", identity.fingerprint());
    println!("Public key   : {}", identity.public_b58());
    println!("Identity file: {}", config::identity_path().display());
    Ok(())
}

fn cmd_keygen(name: Option<String>, force: bool) -> Result<()> {
    let path = config::identity_path();
    if path.exists() && !force {
        anyhow::bail!(
            "an identity already exists at {} (use --force to overwrite)",
            path.display()
        );
    }
    let identity = Identity::generate(name.unwrap_or_else(identity::random_name))?;
    identity.save(&path)?;
    println!("Created new identity: {}", identity.fingerprint());
    Ok(())
}

async fn cmd_discover(
    mode: TransportMode,
    net: NetworkMode,
    name: Option<String>,
    port: u16,
    secs: u64,
) -> Result<()> {
    let identity = Identity::load_or_create(name)?;
    let mut app = App::new(identity, mode, net, port, None).await?;
    if !app.supports_discovery() {
        println!("Discovery is only available in LAN or Bluetooth mode.");
        return Ok(());
    }
    println!("Scanning for peers for {secs}s ({})...", mode.label());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut seen = HashSet::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            maybe = app.events.recv() => {
                match maybe {
                    Some(AppEvent::PeerDiscovered(p)) => {
                        // De-duplicate: mDNS re-resolves a peer once per interface.
                        if seen.insert(p.id.clone()) {
                            let key = p.public_key_b58.as_deref().unwrap_or("?");
                            println!("  - {}  {}  (key {})", p.display_name, p.addr, key);
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }
    if seen.is_empty() {
        println!("No peers found.");
    }
    println!("Done.");
    Ok(())
}

async fn cmd_listen(
    mode: TransportMode,
    net: NetworkMode,
    name: Option<String>,
    port: u16,
    directory: Option<String>,
    connect: Option<String>,
) -> Result<()> {
    let identity = Identity::load_or_create(name)?;
    let fingerprint = identity.fingerprint();
    let net_label = net.label();
    let mut app = App::new(identity, mode, net, port, directory).await?;
    println!(
        "whispr node online.  mode: {}  net: {}  you: {}",
        mode.label(),
        net_label,
        fingerprint
    );
    if let Some(addr) = connect {
        println!("connecting to {addr}...");
        app.spawn_connect(addr);
    }
    println!("Type a line + Enter to broadcast to all connected peers. Ctrl+C to quit.");

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    // Authenticated keys of peers we've connected to, so we can reconnect them.
    let mut known_keys: HashMap<String, String> = HashMap::new();
    loop {
        tokio::select! {
            maybe = app.events.recv() => {
                match maybe {
                    Some(event) => {
                        match &event {
                            AppEvent::PeerConnected { fingerprint, public_key_b58, .. } => {
                                known_keys.insert(fingerprint.clone(), public_key_b58.clone());
                            }
                            AppEvent::PeerDisconnected { fingerprint } => {
                                if let Some(pk) = known_keys.get(fingerprint) {
                                    app.spawn_reconnect_by_key(fingerprint.clone(), pk.clone());
                                }
                            }
                            _ => {}
                        }
                        print_event(&event);
                    }
                    None => break,
                }
            }
            // Guard keeps the node alive after stdin closes (e.g. when run as a
            // background daemon) instead of exiting on EOF.
            line = stdin.next_line(), if stdin_open => {
                match line {
                    Ok(Some(text)) if !text.trim().is_empty() => {
                        app.broadcast(text).await;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => stdin_open = false,
                }
            }
        }
    }
    Ok(())
}

async fn cmd_send(
    mode: TransportMode,
    net: NetworkMode,
    name: Option<String>,
    port: u16,
    directory: Option<String>,
    addr: String,
    message: String,
) -> Result<()> {
    let identity = Identity::load_or_create(name)?;
    let mut app = App::new(identity, mode, net, port, directory).await?;
    println!("Connecting to {addr}...");
    app.connect(&addr).await?;

    // Wait (briefly) for the secure session to establish.
    let fingerprint = wait_for_connect(&mut app, Duration::from_secs(10))
        .await
        .ok_or_else(|| anyhow::anyhow!("timed out establishing a secure session"))?;

    app.send(&fingerprint, message).await?;
    // Say goodbye and wait for the peer to close, which confirms it processed our
    // message. This is more reliable than a fixed sleep: an abrupt exit can send
    // a TCP RST that drops the last unread frame.
    app.disconnect(&fingerprint).await;
    wait_for_disconnect(&mut app, &fingerprint, Duration::from_secs(5)).await;
    println!("Message sent to {fingerprint}.");
    Ok(())
}

async fn cmd_find(directory: Option<String>, public_key: String) -> Result<()> {
    let directory =
        directory.ok_or_else(|| anyhow::anyhow!("--directory host:port is required"))?;
    let client = DirectoryClient::new(directory);
    match client.lookup(&public_key).await? {
        Some(address) => println!("{public_key} is at {address}"),
        None => println!("{public_key} is not registered or is offline"),
    }
    Ok(())
}

async fn cmd_reach(
    mode: TransportMode,
    net: NetworkMode,
    name: Option<String>,
    port: u16,
    directory: Option<String>,
    public_key: String,
    message: String,
) -> Result<()> {
    if directory.is_none() {
        anyhow::bail!("--directory host:port is required to reach a peer by key");
    }
    let identity = Identity::load_or_create(name)?;
    let mut app = App::new(identity, mode, net, port, directory).await?;
    println!("Looking up {public_key}...");
    app.connect_by_key(&public_key).await?;

    let fingerprint = wait_for_connect(&mut app, Duration::from_secs(15))
        .await
        .ok_or_else(|| anyhow::anyhow!("timed out establishing a secure session"))?;

    app.send(&fingerprint, message).await?;
    app.disconnect(&fingerprint).await;
    wait_for_disconnect(&mut app, &fingerprint, Duration::from_secs(5)).await;
    println!("Message sent to {fingerprint}.");
    Ok(())
}

async fn wait_for_connect(app: &mut App, timeout: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return None,
            maybe = app.events.recv() => {
                match maybe {
                    Some(AppEvent::PeerConnected { fingerprint, .. }) => return Some(fingerprint),
                    Some(AppEvent::Error(e)) => {
                        eprintln!("error: {e}");
                        return None;
                    }
                    Some(_) => {}
                    None => return None,
                }
            }
        }
    }
}

/// Wait until the session with `fingerprint` ends (the peer closed after our
/// `Bye`), or the timeout elapses. Used to confirm delivery before exiting.
async fn wait_for_disconnect(app: &mut App, fingerprint: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            maybe = app.events.recv() => {
                match maybe {
                    Some(AppEvent::PeerDisconnected { fingerprint: fp }) if fp == fingerprint => return,
                    Some(_) => {}
                    None => return,
                }
            }
        }
    }
}

fn print_event(event: &AppEvent) {
    match event {
        AppEvent::PeerConnected { display_name, .. } => println!("* {display_name} connected"),
        AppEvent::PeerUpdated { display_name, fingerprint } => {
            println!("* {display_name} [{fingerprint}]")
        }
        AppEvent::PeerDisconnected { fingerprint } => println!("* {fingerprint} disconnected"),
        AppEvent::Message { from, text, broadcast, .. } => {
            if *broadcast {
                println!("{from} (broadcast): {text}")
            } else {
                println!("{from}: {text}")
            }
        }
        AppEvent::PeerDiscovered(p) => println!("~ discovered {} at {}", p.display_name, p.addr),
        AppEvent::Delivered { .. } => {}
        AppEvent::DeliveryFailed { id, .. } => eprintln!("! message {id} was not delivered"),
        AppEvent::Info(m) => println!("info: {m}"),
        AppEvent::Error(e) => eprintln!("error: {e}"),
    }
}
