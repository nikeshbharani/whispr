//! The interactive, full-screen chat UI built with `ratatui`.
//!
//! It opens with a small setup wizard (name → transport → network), then a chat
//! screen with a sidebar of channels:
//!
//! ```text
//! +------------------------------------------------------------+
//! | you: alice · P8hztGjc                                      |
//! | LAN · direct · -> anon-b2ce                                |
//! +----------------+-------------------------------------------+
//! | Broadcast      | messages for the selected channel         |
//! | * bob (2)      |                                           |
//! | o carol        |                                           |
//! +----------------+-------------------------------------------+
//! | message  (Enter=send · Tab/<>=switch · up/down=scroll)     |
//! +------------------------------------------------------------+
//! ```
//!
//! Each connected peer has its own conversation; unread counts appear next to a
//! peer while you're looking at another channel. A dedicated **Broadcast**
//! channel sends to everyone at once.

use std::collections::{HashMap, HashSet};
use std::io::{stdout, IsTerminal, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::app::{App, AppEvent};
use crate::config::{self, NetworkMode, TransportMode};
use crate::identity::{fingerprint_of, random_name, Identity};

/// Terminal handle used throughout the interactive UI.
type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Don't retry an automatic reconnect to the same peer more often than this.
const RECONNECT_THROTTLE: Duration = Duration::from_secs(4);
/// Cap on retained lines per conversation.
const MAX_LINES: usize = 2000;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Launch the interactive UI: setup wizard, then the chat.
pub async fn run_tui(
    identity: Identity,
    default_mode: TransportMode,
    default_net: NetworkMode,
    listen_port: u16,
    directory: Option<String>,
    autoconnect: Option<String>,
) -> Result<()> {
    if !stdout().is_terminal() || !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "the interactive chat needs a terminal (TTY). For headless/background use, run `whispr listen` instead."
        );
    }
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = run_session(
        &mut terminal,
        identity,
        default_mode,
        default_net,
        listen_port,
        directory,
        autoconnect,
    )
    .await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    terminal: &mut Tui,
    mut identity: Identity,
    default_mode: TransportMode,
    default_net: NetworkMode,
    listen_port: u16,
    directory: Option<String>,
    autoconnect: Option<String>,
) -> Result<()> {
    loop {
        let choice = run_selector(terminal, &identity.display_name, default_mode, &default_net).await?;
        let (name, mode, net) = match choice {
            Some(choice) => choice,
            None => return Ok(()),
        };

        // Apply the chosen name (blank keeps/rolls an anonymous handle), persist it.
        let name = name.trim().to_string();
        if !name.is_empty() {
            identity.display_name = name;
        } else if identity.display_name.is_empty() {
            identity.display_name = random_name();
        }
        let _ = identity.save(&config::identity_path());

        match App::new(identity.clone(), mode, net.clone(), listen_port, directory.clone()).await {
            Ok(mut app) => return run_chat(terminal, &mut app, autoconnect.clone()).await,
            Err(e) => {
                let msg = format!(
                    "Could not start {} mode:\n\n{e}\n\n[any key] choose again    [Esc] quit",
                    mode.label()
                );
                if !run_message_screen(terminal, &msg).await? {
                    return Ok(());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Chat loop
// ---------------------------------------------------------------------------

async fn run_chat(terminal: &mut Tui, app: &mut App, autoconnect: Option<String>) -> Result<()> {
    let mut state = TuiState::new();

    if let Some(addr) = autoconnect {
        state.broadcast.push(format!("connecting to {addr}..."));
        app.spawn_connect(addr);
    }

    let mut input_events = EventStream::new();
    loop {
        terminal.draw(|f| render(f, &mut state, app))?;
        if state.should_quit {
            break;
        }
        tokio::select! {
            maybe_input = input_events.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_input {
                    handle_key(key, &mut state, app).await;
                }
            }
            maybe_event = app.events.recv() => {
                match maybe_event {
                    Some(event) => handle_app_event(event, &mut state, app),
                    None => break,
                }
            }
        }
    }
    Ok(())
}

async fn handle_key(key: KeyEvent, state: &mut TuiState, app: &App) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => state.should_quit = true,
        (_, KeyCode::Esc) => state.should_quit = true,
        (_, KeyCode::Tab) | (_, KeyCode::Right) => state.switch_channel(1),
        (_, KeyCode::BackTab) | (_, KeyCode::Left) => state.switch_channel(-1),
        (_, KeyCode::Up) => state.scroll_up(1),
        (_, KeyCode::Down) => state.scroll_down(1),
        (_, KeyCode::PageUp) => state.scroll_up(10),
        (_, KeyCode::PageDown) => state.scroll_down(10),
        (_, KeyCode::End) => state.follow = true,
        (_, KeyCode::Backspace) => {
            state.input.pop();
        }
        (_, KeyCode::Enter) => {
            let text = std::mem::take(&mut state.input).trim().to_string();
            if text.is_empty() {
                // Connect to the selected, not-yet-connected peer.
                if let Some((name, addr, pk)) = state.selected_connect_target() {
                    state.push_active(format!("connecting to {name}..."));
                    app.spawn_connect_expecting(addr, pk);
                }
                return;
            }
            if let Some(command) = text.strip_prefix('/') {
                run_command(command, state, app).await;
                state.push_active(String::new()); // spacing after command output
            } else {
                send_message(text, state, app).await;
            }
        }
        (_, KeyCode::Char(c)) => state.input.push(c),
        _ => {}
    }
}

async fn send_message(text: String, state: &mut TuiState, app: &App) {
    match state.active_target() {
        ActiveTarget::Broadcast => {
            let (id, targets) = app.broadcast(text.clone()).await;
            if targets.is_empty() {
                state.push_active("(no connected peers to broadcast to)".to_string());
            } else {
                let line = state.push_active(format!("me: {text}"));
                state.pending.insert(
                    id,
                    PendingMsg {
                        channel: PendingChannel::Broadcast,
                        line,
                        text,
                        expected: targets.len(),
                        delivered: 0,
                        failed: 0,
                        remaining: targets.into_iter().collect(),
                    },
                );
            }
        }
        ActiveTarget::Peer(fingerprint) => match app.send(&fingerprint, text.clone()).await {
            Ok(id) => {
                let line = state.push_active(format!("me: {text}"));
                let mut remaining = HashSet::new();
                remaining.insert(fingerprint.clone());
                state.pending.insert(
                    id,
                    PendingMsg {
                        channel: PendingChannel::Peer(fingerprint),
                        line,
                        text,
                        expected: 1,
                        delivered: 0,
                        failed: 0,
                        remaining,
                    },
                );
            }
            Err(e) => {
                state.push_active(format!("send failed: {e}"));
            }
        },
        ActiveTarget::Offline(name) => {
            state.push_active(format!(
                "{name} isn't connected — press Enter on an empty line to connect"
            ));
        }
    }
}

async fn run_command(command: &str, state: &mut TuiState, app: &App) {
    let mut parts = command.split_whitespace();
    match parts.next() {
        Some("connect") => match parts.next() {
            Some(addr) => {
                state.push_active(format!("connecting to {addr}..."));
                app.spawn_connect(addr.to_string());
            }
            None => {
                state.push_active("usage: /connect host:port".to_string());
            }
        },
        Some("find") => match parts.next() {
            Some(public_key) => {
                state.push_active(format!("looking up {public_key} via directory..."));
                if let Err(e) = app.connect_by_key(public_key).await {
                    state.push_active(format!("find error: {e}"));
                }
            }
            None => {
                state.push_active("usage: /find <public-key>".to_string());
            }
        },
        Some("discover") => {
            let discovered = app.discovered.lock().await;
            if discovered.is_empty() {
                state.push_active("no peers discovered yet".to_string());
            } else {
                for peer in discovered.values() {
                    state.push_active(format!("discovered: {} at {}", peer.display_name, peer.addr));
                }
            }
        }
        Some("whoami") => {
            state.push_active(format!(
                "you are {} [{}]",
                app.identity.display_name,
                app.identity.fingerprint()
            ));
        }
        Some("quit") | Some("q") => state.should_quit = true,
        Some("help") => {
            state.push_active(
                "commands: /connect host:port · /find <public-key> · /discover · /whoami · /quit"
                    .to_string(),
            );
            state.push_active(
                "keys: Enter=send · Tab or <-/->=switch channel · up/down=scroll · Esc=quit"
                    .to_string(),
            );
        }
        _ => {
            state.push_active("unknown command — try /help".to_string());
        }
    }
}

fn handle_app_event(event: AppEvent, state: &mut TuiState, app: &App) {
    match event {
        AppEvent::PeerConnected { fingerprint, public_key_b58, .. } => {
            state.mark_connected(fingerprint.clone());
            // Remember the authenticated key so we can reconnect after a drop.
            if let Some(peer) = state.peer_mut(&fingerprint) {
                peer.public_key_b58 = Some(public_key_b58);
            }
        }
        AppEvent::PeerUpdated {
            fingerprint,
            display_name,
        } => {
            state.set_name(&fingerprint, &display_name);
            if let Some(peer) = state.peer_mut(&fingerprint) {
                peer.conv.push(format!("* {display_name} connected"));
            }
        }
        AppEvent::PeerDisconnected { fingerprint } => {
            if let Some(peer) = state.peer_mut(&fingerprint) {
                let name = peer.name.clone();
                peer.connected = false;
                peer.conv.push(format!("* {name} disconnected"));
                // On the Internet transport, start trying to re-establish the
                // session through the directory (seamless across network changes).
                if let Some(pk) = peer.public_key_b58.clone() {
                    app.spawn_reconnect_by_key(fingerprint.clone(), pk.clone());
                    // Also redial its last known address (LAN / manual) right away.
                    if let Some(addr) = peer.addr.clone() {
                        peer.last_reconnect = Some(Instant::now());
                        app.spawn_redial(fingerprint.clone(), addr, pk);
                    }
                }
            }
        }
        AppEvent::Message { from, text, broadcast: is_broadcast, .. } => {
            if is_broadcast {
                // Broadcasts belong in everyone's Broadcast channel, attributed
                // to the sender, not in the 1:1 conversation.
                let name = state
                    .peer_mut(&from)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| short(&from));
                state.broadcast.push(format!("{name}: {text}"));
                if state.selected != 0 {
                    state.broadcast.unread += 1;
                }
            } else {
                let active = state.active_peer_fp();
                if let Some(peer) = state.peer_mut(&from) {
                    let name = peer.name.clone();
                    peer.conv.push(format!("{name}: {text}"));
                    if active.as_deref() != Some(&from) {
                        peer.conv.unread += 1;
                    }
                }
            }
        }
        AppEvent::PeerDiscovered(peer) => {
            let Some(pk) = peer.public_key_b58.clone() else {
                return;
            };
            let Ok(raw) = bs58::decode(&pk).into_vec() else {
                return;
            };
            let fingerprint = fingerprint_of(&raw);
            state.upsert_discovered(fingerprint.clone(), peer.display_name, peer.addr.clone(), pk.clone());

            // Seamlessly reconnect to peers we were talking to when they return.
            if let Some(p) = state.peer_mut(&fingerprint) {
                if p.was_connected && !p.connected && p.reconnect_ready() {
                    p.last_reconnect = Some(Instant::now());
                    app.spawn_connect_expecting(peer.addr, pk);
                }
            }
        }
        AppEvent::Delivered { peer, id } => state.resolve_delivery(id, &peer, true),
        AppEvent::DeliveryFailed { peer, id } => state.resolve_delivery(id, &peer, false),
        AppEvent::Info(message) => {
            state.broadcast.push(format!("· {message}"));
        }
        AppEvent::Error(error) => {
            state.broadcast.push(format!("error: {error}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, state: &mut TuiState, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(f.area());

    // Status bar — two short, wrapping lines so it fits narrow (phone) screens.
    let line1 = format!(
        "you: {} · {}",
        app.identity.display_name,
        short(&app.identity.fingerprint())
    );
    let line2 = format!(
        "{} · {} · -> {}",
        mode_short(app.mode),
        net_short(&app.net),
        state.active_label()
    );
    f.render_widget(
        Paragraph::new(format!("{line1}\n{line2}"))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("whispr")),
        rows[0],
    );

    // Middle: channel sidebar + messages.
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(rows[1]);

    f.render_widget(
        List::new(state.sidebar_items()).block(
            Block::default()
                .borders(Borders::ALL)
                .title("channels [Tab]"),
        ),
        middle[0],
    );

    // Messages: pre-wrap to the exact width so auto-tail lands on the last line.
    let msg_area = middle[1];
    let inner_w = msg_area.width.saturating_sub(2) as usize;
    let inner_h = msg_area.height.saturating_sub(2);
    let lines = wrap_lines(state.active_conv().lines(), inner_w);
    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner_h);
    state.last_max_scroll = max_scroll;
    let scroll_y = if state.follow {
        max_scroll
    } else {
        state.scroll_top = state.scroll_top.min(max_scroll);
        state.scroll_top
    };
    let title = if state.follow {
        state.active_label()
    } else {
        format!("{} (scrolled — End=follow)", state.active_label())
    };
    f.render_widget(
        Paragraph::new(lines.join("\n"))
            .scroll((scroll_y, 0))
            .block(Block::default().borders(Borders::ALL).title(title)),
        msg_area,
    );

    // Input — horizontally scroll so the end (and cursor) is always visible.
    let input_area = rows[2];
    let input_w = input_area.width.saturating_sub(2) as usize;
    let chars: Vec<char> = state.input.chars().collect();
    let shown: String = if chars.len() > input_w {
        chars[chars.len() - input_w..].iter().collect()
    } else {
        state.input.clone()
    };
    let cursor_col = chars.len().min(input_w) as u16;
    f.render_widget(
        Paragraph::new(shown).block(
            Block::default()
                .borders(Borders::ALL)
                .title("message  (Enter=send · Tab=switch · Esc=quit)"),
        ),
        input_area,
    );
    f.set_cursor_position((input_area.x + 1 + cursor_col, input_area.y + 1));
}

fn mode_short(mode: TransportMode) -> &'static str {
    match mode {
        TransportMode::Lan => "LAN",
        TransportMode::Internet => "Internet",
        TransportMode::Bluetooth => "BLE",
    }
}

fn net_short(net: &NetworkMode) -> &'static str {
    match net {
        NetworkMode::Direct => "direct",
        NetworkMode::Tor => "tor",
        NetworkMode::Proxy(_) => "proxy",
    }
}

/// First 8 characters of a fingerprint, for compact display.
fn short(fingerprint: &str) -> String {
    fingerprint.chars().take(8).collect()
}

/// Word-wrap a message to `width` columns, hard-splitting over-long words.
fn wrap_message(msg: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![msg.to_string()];
    }
    if msg.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in msg.split(' ') {
        let wlen = word.chars().count();
        if wlen > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            let chars: Vec<char> = word.chars().collect();
            for chunk in chars.chunks(width) {
                lines.push(chunk.iter().collect());
            }
            continue;
        }
        let add = if cur.is_empty() { wlen } else { wlen + 1 };
        if cur_len + add > width {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
            cur_len = wlen;
        } else {
            if !cur.is_empty() {
                cur.push(' ');
                cur_len += 1;
            }
            cur.push_str(word);
            cur_len += wlen;
        }
    }
    lines.push(cur);
    lines
}

fn wrap_lines(messages: &[String], width: usize) -> Vec<String> {
    messages
        .iter()
        .flat_map(|m| wrap_message(m, width))
        .collect()
}

// ---------------------------------------------------------------------------
// Setup wizard (name → transport → network → proxy)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectStep {
    Name,
    Transport,
    Network,
    ProxyInput,
}

enum Outcome {
    Done(String, TransportMode, NetworkMode),
    Quit,
}

struct Selector {
    step: SelectStep,
    name: String,
    transport_idx: usize,
    network_idx: usize,
    proxy_input: String,
    outcome: Option<Outcome>,
}

const TRANSPORT_LABELS: [&str; 3] = ["Wi-Fi / Local network", "Internet", "Bluetooth"];
const TRANSPORT_DESC: [&str; 3] = [
    "Offline. Finds peers on your Wi-Fi/LAN automatically.",
    "Peer-to-peer over the internet. Choose privacy routing next.",
    "Experimental — Bluetooth chat is not usable yet.",
];
const NETWORK_LABELS: [&str; 3] = ["Direct", "Tor", "Proxy"];
const NETWORK_DESC: [&str; 3] = [
    "Fastest. The peer can see your IP address.",
    "Anonymous. Routes through local Tor (127.0.0.1:9050).",
    "Route through a custom SOCKS5 proxy you enter next.",
];

impl Selector {
    fn new(default_name: &str, default_mode: TransportMode, default_net: &NetworkMode) -> Self {
        let transport_idx = match default_mode {
            TransportMode::Lan => 0,
            TransportMode::Internet => 1,
            TransportMode::Bluetooth => 2,
        };
        let (network_idx, proxy_input) = match default_net {
            NetworkMode::Direct => (0, String::new()),
            NetworkMode::Tor => (1, String::new()),
            NetworkMode::Proxy(addr) => (2, addr.clone()),
        };
        Self {
            step: SelectStep::Name,
            name: default_name.to_string(),
            transport_idx,
            network_idx,
            proxy_input,
            outcome: None,
        }
    }

    fn take_outcome(&mut self) -> Option<Outcome> {
        self.outcome.take()
    }

    fn on_key(&mut self, key: KeyEvent) {
        match self.step {
            SelectStep::Name => match key.code {
                KeyCode::Char(c) => self.name.push(c),
                KeyCode::Backspace => {
                    self.name.pop();
                }
                KeyCode::Enter => self.step = SelectStep::Transport,
                KeyCode::Esc => self.outcome = Some(Outcome::Quit),
                _ => {}
            },
            SelectStep::Transport => match key.code {
                KeyCode::Up => self.transport_idx = (self.transport_idx + 2) % 3,
                KeyCode::Down => self.transport_idx = (self.transport_idx + 1) % 3,
                KeyCode::Enter => match self.transport_idx {
                    0 => self.finish(TransportMode::Lan, NetworkMode::Direct),
                    2 => self.finish(TransportMode::Bluetooth, NetworkMode::Direct),
                    _ => self.step = SelectStep::Network,
                },
                KeyCode::Esc => self.step = SelectStep::Name,
                _ => {}
            },
            SelectStep::Network => match key.code {
                KeyCode::Up => self.network_idx = (self.network_idx + 2) % 3,
                KeyCode::Down => self.network_idx = (self.network_idx + 1) % 3,
                KeyCode::Enter => match self.network_idx {
                    0 => self.finish(TransportMode::Internet, NetworkMode::Direct),
                    1 => self.finish(TransportMode::Internet, NetworkMode::Tor),
                    _ => self.step = SelectStep::ProxyInput,
                },
                KeyCode::Esc => self.step = SelectStep::Transport,
                _ => {}
            },
            SelectStep::ProxyInput => match key.code {
                KeyCode::Char(c) => self.proxy_input.push(c),
                KeyCode::Backspace => {
                    self.proxy_input.pop();
                }
                KeyCode::Enter => {
                    let addr = self.proxy_input.trim().to_string();
                    if !addr.is_empty() {
                        self.finish(TransportMode::Internet, NetworkMode::Proxy(addr));
                    }
                }
                KeyCode::Esc => self.step = SelectStep::Network,
                _ => {}
            },
        }
    }

    fn finish(&mut self, mode: TransportMode, net: NetworkMode) {
        self.outcome = Some(Outcome::Done(self.name.clone(), mode, net));
    }
}

async fn run_selector(
    terminal: &mut Tui,
    default_name: &str,
    default_mode: TransportMode,
    default_net: &NetworkMode,
) -> Result<Option<(String, TransportMode, NetworkMode)>> {
    let mut selector = Selector::new(default_name, default_mode, default_net);
    let mut events = EventStream::new();
    loop {
        terminal.draw(|f| render_selector(f, &selector))?;
        if let Some(outcome) = selector.take_outcome() {
            return Ok(match outcome {
                Outcome::Done(name, mode, net) => Some((name, mode, net)),
                Outcome::Quit => None,
            });
        }
        if let Some(Ok(Event::Key(key))) = events.next().await {
            if key.kind == KeyEventKind::Press {
                selector.on_key(key);
            }
        }
    }
}

fn render_selector(f: &mut Frame, selector: &Selector) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(f.area());

    let title = match selector.step {
        SelectStep::Name => "What name should others see?",
        SelectStep::Transport => "How do you want to connect?",
        SelectStep::Network => "Network privacy (Internet)",
        SelectStep::ProxyInput => "Enter a SOCKS5 proxy address",
    };
    f.render_widget(
        Paragraph::new(title)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("whispr — setup")),
        rows[0],
    );

    match selector.step {
        SelectStep::Name => render_input(f, rows[1], "your name (blank = random)", &selector.name),
        SelectStep::ProxyInput => {
            render_input(f, rows[1], "host:port (e.g. 127.0.0.1:1080)", &selector.proxy_input)
        }
        SelectStep::Transport => render_choice(
            f,
            rows[1],
            &TRANSPORT_LABELS,
            &TRANSPORT_DESC,
            selector.transport_idx,
        ),
        SelectStep::Network => render_choice(
            f,
            rows[1],
            &NETWORK_LABELS,
            &NETWORK_DESC,
            selector.network_idx,
        ),
    }

    let help = match selector.step {
        SelectStep::Name => "type a name · Enter = continue · Esc = quit",
        SelectStep::ProxyInput => "type host:port · Enter = confirm · Esc = back",
        _ => "Up/Down = move · Enter = select · Esc = back",
    };
    f.render_widget(
        Paragraph::new(help)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL)),
        rows[2],
    );
}

fn render_input(f: &mut Frame, area: Rect, title: &str, value: &str) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let chars: Vec<char> = value.chars().collect();
    let shown: String = if inner_w > 0 && chars.len() > inner_w {
        chars[chars.len() - inner_w..].iter().collect()
    } else {
        value.to_string()
    };
    let cursor = chars.len().min(inner_w) as u16;
    f.render_widget(
        Paragraph::new(shown).block(Block::default().borders(Borders::ALL).title(title.to_string())),
        area,
    );
    f.set_cursor_position((area.x + 1 + cursor, area.y + 1));
}

/// A short option list plus a wrapping description of the highlighted option —
/// stays readable on narrow (portrait) terminals.
fn render_choice(f: &mut Frame, area: Rect, labels: &[&str], descs: &[&str], selected: usize) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(area);

    let items: Vec<ListItem> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let marker = if i == selected { "> " } else { "  " };
            ListItem::new(format!("{marker}{label}"))
        })
        .collect();
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL)),
        parts[0],
    );

    let desc = descs.get(selected).copied().unwrap_or("");
    f.render_widget(
        Paragraph::new(desc)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("about")),
        parts[1],
    );
}

/// Show a full-screen message and wait for a key. Returns `false` on Esc.
async fn run_message_screen(terminal: &mut Tui, message: &str) -> Result<bool> {
    let mut events = EventStream::new();
    loop {
        terminal.draw(|f| {
            let area = centered_rect(80, 50, f.area());
            f.render_widget(
                Paragraph::new(message)
                    .wrap(Wrap { trim: false })
                    .alignment(Alignment::Center)
                    .block(Block::default().borders(Borders::ALL).title("whispr")),
                area,
            );
        })?;
        if let Some(Ok(Event::Key(key))) = events.next().await {
            if key.kind == KeyEventKind::Press {
                return Ok(!matches!(key.code, KeyCode::Esc));
            }
        }
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

/// A scrollback buffer for one channel.
struct Conversation {
    lines: Vec<String>,
    unread: usize,
    /// Total lines ever dropped from the front (for stable line addressing when
    /// updating a message's delivery status after old lines are pruned).
    dropped: usize,
}

impl Conversation {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            unread: 0,
            dropped: 0,
        }
    }
    fn welcome() -> Self {
        Self {
            lines: vec![
                "Welcome to whispr. Everything is end-to-end encrypted.".to_string(),
                "Select a channel with Tab, or a discovered peer + Enter to connect.".to_string(),
            ],
            unread: 0,
            dropped: 0,
        }
    }
    /// Append a line and return its stable index (usable with [`set_line`]).
    fn push(&mut self, line: String) -> usize {
        self.lines.push(line);
        if self.lines.len() > MAX_LINES {
            let removed = MAX_LINES / 2;
            self.lines.drain(0..removed);
            self.dropped += removed;
        }
        self.dropped + self.lines.len() - 1
    }
    /// Overwrite a previously-pushed line by its stable index. A no-op if the
    /// line has since been pruned.
    fn set_line(&mut self, index: usize, text: String) {
        if let Some(rel) = index.checked_sub(self.dropped) {
            if let Some(slot) = self.lines.get_mut(rel) {
                *slot = text;
            }
        }
    }
    fn lines(&self) -> &[String] {
        &self.lines
    }
}

/// A peer channel: keyed by fingerprint, with its own conversation.
struct Peer {
    fingerprint: String,
    name: String,
    public_key_b58: Option<String>,
    addr: Option<String>,
    connected: bool,
    was_connected: bool,
    conv: Conversation,
    last_reconnect: Option<Instant>,
}

impl Peer {
    fn reconnect_ready(&self) -> bool {
        self.addr.is_some()
            && self.public_key_b58.is_some()
            && self
                .last_reconnect
                .is_none_or(|t| t.elapsed() > RECONNECT_THROTTLE)
    }
}

enum ActiveTarget {
    Broadcast,
    Peer(String),
    Offline(String),
}

/// Which conversation a sent message's status line lives in.
enum PendingChannel {
    Broadcast,
    Peer(String),
}

/// A message we've sent that is still awaiting delivery confirmation from one or
/// more recipients.
struct PendingMsg {
    channel: PendingChannel,
    /// Stable index of the "me: …" line to annotate with the final status.
    line: usize,
    text: String,
    expected: usize,
    delivered: usize,
    failed: usize,
    /// Recipients that have not yet acked or timed out.
    remaining: HashSet<String>,
}

struct TuiState {
    input: String,
    broadcast: Conversation,
    peers: Vec<Peer>,
    /// 0 = broadcast, otherwise `peers[selected - 1]`.
    selected: usize,
    should_quit: bool,
    follow: bool,
    scroll_top: u16,
    last_max_scroll: u16,
    /// Sent messages awaiting delivery receipts, keyed by message id.
    pending: HashMap<u64, PendingMsg>,
}

impl TuiState {
    fn new() -> Self {
        Self {
            input: String::new(),
            broadcast: Conversation::welcome(),
            peers: Vec::new(),
            selected: 0,
            should_quit: false,
            follow: true,
            scroll_top: 0,
            last_max_scroll: 0,
            pending: HashMap::new(),
        }
    }

    fn peer_mut(&mut self, fingerprint: &str) -> Option<&mut Peer> {
        self.peers.iter_mut().find(|p| p.fingerprint == fingerprint)
    }

    fn mark_connected(&mut self, fingerprint: String) {
        if let Some(p) = self.peer_mut(&fingerprint) {
            p.connected = true;
            p.was_connected = true;
        } else {
            let name = short(&fingerprint);
            self.peers.push(Peer {
                fingerprint,
                name,
                public_key_b58: None,
                addr: None,
                connected: true,
                was_connected: true,
                conv: Conversation::new(),
                last_reconnect: None,
            });
        }
    }

    fn set_name(&mut self, fingerprint: &str, name: &str) {
        if let Some(p) = self.peer_mut(fingerprint) {
            p.name = name.to_string();
        }
    }

    fn upsert_discovered(
        &mut self,
        fingerprint: String,
        name: String,
        addr: String,
        pk: String,
    ) {
        if let Some(p) = self.peer_mut(&fingerprint) {
            p.addr = Some(addr);
            p.public_key_b58 = Some(pk);
            if !p.connected {
                p.name = name;
            }
        } else {
            self.peers.push(Peer {
                fingerprint,
                name,
                public_key_b58: Some(pk),
                addr: Some(addr),
                connected: false,
                was_connected: false,
                conv: Conversation::new(),
                last_reconnect: None,
            });
        }
    }

    fn switch_channel(&mut self, delta: isize) {
        let total = self.peers.len() as isize + 1;
        if total == 0 {
            return;
        }
        let next = (self.selected as isize + delta).rem_euclid(total);
        self.select(next as usize);
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.peers.len());
        self.follow = true;
        self.scroll_top = 0;
        // Clear unread on the now-active channel.
        if self.selected == 0 {
            self.broadcast.unread = 0;
        } else if let Some(p) = self.peers.get_mut(self.selected - 1) {
            p.conv.unread = 0;
        }
    }

    fn active_conv(&self) -> &Conversation {
        if self.selected == 0 {
            &self.broadcast
        } else {
            self.peers
                .get(self.selected - 1)
                .map(|p| &p.conv)
                .unwrap_or(&self.broadcast)
        }
    }

    fn push_active(&mut self, line: String) -> usize {
        if self.selected == 0 {
            self.broadcast.push(line)
        } else if let Some(p) = self.peers.get_mut(self.selected - 1) {
            p.conv.push(line)
        } else {
            self.broadcast.push(line)
        }
    }

    /// Record an ack (or timeout) for a sent message and, once every recipient
    /// has responded, annotate its line with a delivery receipt.
    fn resolve_delivery(&mut self, id: u64, peer: &str, delivered: bool) {
        let Some(p) = self.pending.get_mut(&id) else {
            return;
        };
        if p.remaining.remove(peer) {
            if delivered {
                p.delivered += 1;
            } else {
                p.failed += 1;
            }
        }
        if !p.remaining.is_empty() {
            return;
        }
        let done = self.pending.remove(&id).unwrap();
        let status = if done.failed == 0 {
            format!("me: {}  \u{2713}", done.text)
        } else if done.delivered == 0 {
            format!("me: {}  \u{26a0} not delivered", done.text)
        } else {
            format!(
                "me: {}  \u{26a0} delivered to {}/{}",
                done.text, done.delivered, done.expected
            )
        };
        match done.channel {
            PendingChannel::Broadcast => self.broadcast.set_line(done.line, status),
            PendingChannel::Peer(fp) => {
                if let Some(peer) = self.peer_mut(&fp) {
                    peer.conv.set_line(done.line, status);
                }
            }
        }
    }

    fn active_peer_fp(&self) -> Option<String> {
        if self.selected == 0 {
            None
        } else {
            self.peers.get(self.selected - 1).map(|p| p.fingerprint.clone())
        }
    }

    fn active_target(&self) -> ActiveTarget {
        if self.selected == 0 {
            return ActiveTarget::Broadcast;
        }
        match self.peers.get(self.selected - 1) {
            Some(p) if p.connected => ActiveTarget::Peer(p.fingerprint.clone()),
            Some(p) => ActiveTarget::Offline(p.name.clone()),
            None => ActiveTarget::Broadcast,
        }
    }

    /// `(name, addr, pk)` if the selected peer can be connected to right now.
    fn selected_connect_target(&self) -> Option<(String, String, String)> {
        if self.selected == 0 {
            return None;
        }
        let p = self.peers.get(self.selected - 1)?;
        if p.connected {
            return None;
        }
        Some((p.name.clone(), p.addr.clone()?, p.public_key_b58.clone()?))
    }

    fn active_label(&self) -> String {
        if self.selected == 0 {
            "Broadcast".to_string()
        } else {
            match self.peers.get(self.selected - 1) {
                Some(p) => p.name.clone(),
                None => "Broadcast".to_string(),
            }
        }
    }

    fn sidebar_items(&self) -> Vec<ListItem<'static>> {
        let mut items = Vec::with_capacity(self.peers.len() + 1);
        let cursor = if self.selected == 0 { ">" } else { " " };
        let unread = if self.broadcast.unread > 0 {
            format!(" ({})", self.broadcast.unread)
        } else {
            String::new()
        };
        items.push(ListItem::new(format!("{cursor}= Broadcast{unread}")));

        for (i, p) in self.peers.iter().enumerate() {
            let cursor = if self.selected == i + 1 { ">" } else { " " };
            let dot = if p.connected { '\u{25cf}' } else { '\u{25cb}' };
            let unread = if p.conv.unread > 0 {
                format!(" ({})", p.conv.unread)
            } else {
                String::new()
            };
            items.push(ListItem::new(format!("{cursor}{dot} {}{unread}", p.name)));
        }
        items
    }

    fn scroll_up(&mut self, lines: u16) {
        if self.follow {
            self.scroll_top = self.last_max_scroll;
            self.follow = false;
        }
        self.scroll_top = self.scroll_top.saturating_sub(lines);
    }

    fn scroll_down(&mut self, lines: u16) {
        if self.follow {
            return;
        }
        self.scroll_top = self.scroll_top.saturating_add(lines);
        if self.scroll_top >= self.last_max_scroll {
            self.follow = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(selector: &mut Selector, code: KeyCode) {
        selector.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_str(selector: &mut Selector, s: &str) {
        for c in s.chars() {
            press(selector, KeyCode::Char(c));
        }
    }

    fn done(selector: &mut Selector) -> (String, TransportMode, NetworkMode) {
        match selector.take_outcome() {
            Some(Outcome::Done(name, mode, net)) => (name, mode, net),
            _ => panic!("expected a completed selection"),
        }
    }

    #[test]
    fn name_then_lan() {
        let mut s = Selector::new("", TransportMode::Lan, &NetworkMode::Direct);
        type_str(&mut s, "alice");
        press(&mut s, KeyCode::Enter); // name -> transport
        press(&mut s, KeyCode::Enter); // LAN
        let (name, mode, net) = done(&mut s);
        assert_eq!(name, "alice");
        assert_eq!(mode, TransportMode::Lan);
        assert_eq!(net, NetworkMode::Direct);
    }

    #[test]
    fn blank_name_kept_blank_for_caller() {
        let mut s = Selector::new("", TransportMode::Lan, &NetworkMode::Direct);
        press(&mut s, KeyCode::Enter); // accept blank name
        press(&mut s, KeyCode::Enter); // LAN
        let (name, _, _) = done(&mut s);
        assert_eq!(name, "");
    }

    #[test]
    fn internet_then_tor() {
        let mut s = Selector::new("bob", TransportMode::Lan, &NetworkMode::Direct);
        press(&mut s, KeyCode::Enter); // name -> transport
        press(&mut s, KeyCode::Down); // Internet
        press(&mut s, KeyCode::Enter); // -> network
        press(&mut s, KeyCode::Down); // Tor
        press(&mut s, KeyCode::Enter);
        let (_, mode, net) = done(&mut s);
        assert_eq!(mode, TransportMode::Internet);
        assert_eq!(net, NetworkMode::Tor);
    }

    #[test]
    fn internet_proxy_typed() {
        let mut s = Selector::new("bob", TransportMode::Lan, &NetworkMode::Direct);
        press(&mut s, KeyCode::Enter); // name -> transport
        press(&mut s, KeyCode::Down); // Internet
        press(&mut s, KeyCode::Enter); // -> network
        press(&mut s, KeyCode::Down); // Tor
        press(&mut s, KeyCode::Down); // Proxy
        press(&mut s, KeyCode::Enter); // -> proxy input
        type_str(&mut s, "127.0.0.1:1080");
        press(&mut s, KeyCode::Enter);
        let (_, mode, net) = done(&mut s);
        assert_eq!(mode, TransportMode::Internet);
        assert_eq!(net, NetworkMode::Proxy("127.0.0.1:1080".to_string()));
    }

    #[test]
    fn esc_from_name_quits() {
        let mut s = Selector::new("x", TransportMode::Lan, &NetworkMode::Direct);
        press(&mut s, KeyCode::Esc);
        assert!(matches!(s.take_outcome(), Some(Outcome::Quit)));
    }

    #[test]
    fn wrap_tail_is_exact() {
        // A message longer than the width should wrap into multiple lines.
        let lines = wrap_message("hello world this is a long line", 10);
        assert!(lines.len() >= 3);
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
    }

    #[test]
    fn conversations_track_unread_and_switch() {
        let mut st = TuiState::new();
        st.mark_connected("FP1".to_string());
        st.set_name("FP1", "bob");
        st.mark_connected("FP2".to_string());
        st.set_name("FP2", "carol");
        // Viewing broadcast (0); a message from bob should mark bob unread.
        let active = st.active_peer_fp();
        assert_eq!(active, None);
        if let Some(p) = st.peer_mut("FP1") {
            p.conv.push("bob: hi".to_string());
            p.conv.unread += 1;
        }
        assert_eq!(st.peers[0].conv.unread, 1);
        // Switch to bob clears unread.
        st.select(1);
        assert_eq!(st.peers[0].conv.unread, 0);
        assert!(matches!(st.active_target(), ActiveTarget::Peer(_)));
    }

    fn track_pending(st: &mut TuiState, id: u64, fp: &str, text: &str) {
        st.select(1);
        let line = st.push_active(format!("me: {text}"));
        let remaining: HashSet<String> = std::iter::once(fp.to_string()).collect();
        st.pending.insert(
            id,
            PendingMsg {
                channel: PendingChannel::Peer(fp.to_string()),
                line,
                text: text.to_string(),
                expected: 1,
                delivered: 0,
                failed: 0,
                remaining,
            },
        );
    }

    #[test]
    fn a_delivered_message_gets_a_check_mark() {
        let mut st = TuiState::new();
        st.mark_connected("FP1".to_string());
        st.set_name("FP1", "bob");
        track_pending(&mut st, 7, "FP1", "hi");

        st.resolve_delivery(7, "FP1", true);

        let last = st.peers[0].conv.lines().last().cloned().unwrap();
        assert!(last.contains('\u{2713}'), "expected a check mark: {last}");
        assert!(st.pending.is_empty(), "resolved message should clear pending");
    }

    #[test]
    fn an_undelivered_message_is_flagged() {
        let mut st = TuiState::new();
        st.mark_connected("FP1".to_string());
        st.set_name("FP1", "bob");
        track_pending(&mut st, 9, "FP1", "hi");

        st.resolve_delivery(9, "FP1", false);

        let last = st.peers[0].conv.lines().last().cloned().unwrap();
        assert!(last.contains("not delivered"), "expected a failure flag: {last}");
        assert!(st.pending.is_empty());
    }
}
