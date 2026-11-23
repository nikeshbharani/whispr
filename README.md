# whispr

A **serverless, end-to-end encrypted, anonymous peer-to-peer messenger**.
No accounts, no phone numbers, no central server — your public key is your
address. It runs as an interactive terminal chat (TUI) and as a scriptable CLI,
and works the same on desktops and (via the terminal) anywhere Rust runs.

## Modes

You choose how messages travel:

| Mode | Internet? | How peers are found | Anonymity options |
|------|-----------|---------------------|-------------------|
| **LAN** | No | Automatic (mDNS / Bonjour) | Local-only by design |
| **Internet** | Yes | Manual address / `.onion` | `direct`, `tor`, `proxy` |
| **Bluetooth** | No | BLE scan (experimental, not usable yet) | Local-only by design |

End-to-end encryption is **always on** in every mode. The network mode only
controls how the underlying connection is routed (e.g. through Tor to hide
your IP).

## Why Rust (not C++)?

This is a security-sensitive networking tool, and Rust is the better fit:

- **Memory safety without a GC** — no buffer overflows/use-after-free, the
  class of bugs behind most messenger CVEs.
- **First-class async networking** with `tokio`.
- **Audited crypto** via the Noise Protocol (`snow`).
- **Great terminal UI** (`ratatui`) and CLI (`clap`) libraries.
- **Cross-platform**, including a path to mobile via `uniffi`/`cargo-ndk`.

## Security model

- **Handshake:** Noise `XX_25519_ChaChaPoly_BLAKE2s`. Both sides exchange and
  authenticate static X25519 keys during the handshake, giving mutual
  authentication and **forward secrecy** (ephemeral keys per session).
- **Identity = key pair.** No PII. A short BLAKE-of-key *fingerprint* is shown
  for out-of-band verification (compare it over a trusted channel to defeat
  man-in-the-middle attacks).
- **Anonymity.** With `--net tor` (or a SOCKS5 `--proxy`), outbound connections
  are routed so peers never see your IP; you can also dial `.onion` addresses.
- **Local key storage.** Your private key lives in a `0600` file under your OS
  data directory and never leaves the process.

> Verify a contact's fingerprint out-of-band the first time you talk. This is
> what makes the encryption trustworthy end-to-end.

## Architecture

Small, single-responsibility modules; transports are pluggable behind one trait.

```
+-----------------------------------------------------+
|              UI:  CLI (clap)  +  TUI (ratatui)       |   src/ui/
+-----------------------------------------------------+
|        App orchestrator (events, connect, send)     |   src/app.rs
+-----------------------------------------------------+
|   Session / connection manager (per-peer tasks)     |   src/session.rs
+-----------------------------------------------------+
|   Crypto: Noise handshake + transport (E2E)         |   src/crypto/
+-----------------------------------------------------+
|          Transport trait  (Send/Recv raw bytes)     |   src/transport/
|   LAN (mDNS+TCP) | Internet (TCP + Tor/SOCKS5) | BLE |
+-----------------------------------------------------+
|   Identity (X25519)   |   Wire protocol   |  Config  |
+-----------------------------------------------------+
```

Adding a new transport means implementing one trait (`Transport`) — everything
above it (encryption, sessions, both UIs) is reused unchanged.

## Install

whispr is a single self-contained Rust binary, so installing it means building
it once with `cargo` (or copying a prebuilt binary). Verified on macOS (arm64);
Linux and Windows use the identical commands, and Android works through Termux.

### Prerequisite: the Rust toolchain

Install `rustup` (stable) if you don't have it — https://rustup.rs

```bash
# macOS / Linux / Termux
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On **Windows**, download and run `rustup-init.exe` from https://rustup.rs
(installs the MSVC toolchain).

### Quickest install (all desktop platforms)

From the project directory:

```bash
cargo install --path .
```

This builds an optimized release binary and installs it to Cargo's bin
directory, which is already on your `PATH`:

- macOS / Linux: `~/.cargo/bin/whispr`
- Windows: `%USERPROFILE%\.cargo\bin\whispr.exe`

Then just run `whispr` from anywhere. Verify with:

```bash
whispr --version
whispr id
```

To include the experimental Bluetooth transport:

```bash
cargo install --path . --features bluetooth
```

### Per-platform notes

**macOS**
```bash
cargo install --path .
whispr                # launches the interactive TUI (LAN mode)
```

**Linux**
```bash
# Debian/Ubuntu: a C toolchain is needed to link
sudo apt-get install -y build-essential
cargo install --path .
```

**Arch Linux / Manjaro / EndeavourOS**
```bash
# base-devel provides the linker/C toolchain; rust comes from the official repo
# (or use rustup instead: sudo pacman -S rustup && rustup default stable)
sudo pacman -S --needed base-devel rust git
cargo install --path .
```
Make sure `~/.cargo/bin` is on your `PATH`.

**Running inside a VM (any Linux distro):** LAN discovery uses mDNS multicast,
which does not cross a NAT. Set the VM's network adapter to **Bridged** (so it
gets its own IP on your LAN) — with the default NAT mode, the VM cannot see or be
seen by other devices. Also allow UDP 5353 (mDNS) and the TCP port you listen on
through the guest/host firewall.

**Windows** (PowerShell)
```powershell
cargo install --path .
whispr --version
```
Note: private-key file-permission hardening is a Unix-only step, so on Windows
the key file relies on your user profile's default ACLs.

**Android (via Termux)**

Termux is a terminal-emulator app that runs a real Linux userland — no root
required.
```bash
# inside the Termux app
pkg update && pkg install rust git
git clone <your-repo-url> whispr && cd whispr
cargo install --path .
whispr id
```
Alternatively, download the prebuilt `whispr-android-arm64` binary from the
project's Releases page (copy it into Termux, `chmod +x`, and run it) instead of
building.
LAN discovery depends on Wi-Fi multicast (works on most home networks; many
mobile-data/carrier networks block it). For internet mode, use a directory
server or a reachable address.

**iOS**

Running an arbitrary CLI is not supported on stock iOS. The path here is a
native app wrapping the same Rust core via `uniffi` (see Roadmap), not the CLI.

### Distributing a prebuilt binary

If you just want to hand someone a binary (no Rust needed on their machine):

```bash
cargo build --release
# copy the single binary to a machine of the SAME OS + CPU architecture:
#   target/release/whispr           (macOS/Linux)
#   target/release/whispr.exe       (Windows)
```

### Prebuilt binaries

Ready-to-run binaries for several platforms are attached to each release on the
project's **Releases** page. They are not stored in the repository: `dist/` is
git-ignored and is only where `./scripts/build-release.sh` writes its output
(see below), so build them yourself if you cloned the source. Each binary is
self-contained (Linux builds are fully static musl); just copy the matching one
to the target device and run it — no Rust required.

| File | Platform | Notes |
|------|----------|-------|
| `whispr-macos-arm64` | macOS, Apple Silicon | `chmod +x`, then run from Terminal |
| `whispr-macos-x86_64` | macOS, Intel | `chmod +x`, then run from Terminal |
| `whispr-linux-x86_64` | Linux, x86-64 | static (musl), runs on any distro |
| `whispr-linux-arm64` | Linux, ARM64 | static (musl); servers, Raspberry Pi |
| `whispr-windows-x86_64.exe` | Windows, x86-64 | run from PowerShell/cmd |
| `whispr-windows-arm64.exe` | Windows on ARM (Snapdragon laptops, Surface Pro X) | run from PowerShell/cmd |
| `whispr-android-arm64` | Android, ARM64 | run inside Termux: `chmod +x`, then `./whispr-android-arm64` |

> **Where are the macOS binaries?** Apple only lets you build macOS binaries
> on a Mac, and the maintainer currently doesn't own one (donations of spare
> MacBooks will be accepted with suspicious enthusiasm). Until then, Mac users
> get the full artisanal experience: install Rust and run
> `cargo install --path .` — it takes about a minute and you get to feel like a
> compiler engineer. If you build them, a PR or release upload is very welcome.

#### Running the macOS binaries

The bundled macOS binaries are ad-hoc code-signed, so they launch as-is. If you
**download** one (or copy it between Macs), macOS Gatekeeper flags it and it may
be killed on launch (`zsh: killed`) or refuse to open. Clear the quarantine and
re-sign once:

```bash
xattr -c whispr-macos-arm64            # remove download quarantine + Finder attrs
codesign --force --sign - whispr-macos-arm64   # ad-hoc re-sign
./whispr-macos-arm64                    # now runs (opens the chat)
```

Tip: it's a terminal app — run it **from Terminal**, not by double-clicking, so
you can see and interact with the chat UI.

To reproduce the binaries yourself, use the recipe below.

To produce binaries for *other* platforms from one machine, use
[`cross`](https://github.com/cross-rs/cross) (Linux/Windows targets) or
[`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk) (Android), e.g.:

```bash
# Windows: needs mingw-w64 (brew install mingw-w64)
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu

# Linux (static): needs a musl cross-linker on PATH
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl

# macOS Intel (from Apple Silicon, no extra toolchain)
rustup target add x86_64-apple-darwin
cargo build --release --target x86_64-apple-darwin

# Android arm64 (needs the Android NDK + cargo-ndk)
cargo install cargo-ndk
rustup target add aarch64-linux-android
ANDROID_NDK_HOME=/path/to/android-ndk cargo ndk -t arm64-v8a -P 24 build --release
```

The cross-linkers are wired up in [`.cargo/config.toml`](.cargo/config.toml).

### One command to build them all

The manual steps above are bundled into a reproducible script:

```bash
./scripts/build-release.sh                  # interactive menu: pick platforms
./scripts/build-release.sh linux android    # build only these (groups or names)
./scripts/build-release.sh linux-x86_64     # a single platform
./scripts/build-release.sh all              # everything this host can build
./scripts/build-release.sh --list           # show available platforms
```

Platforms: `linux-x86_64`, `linux-arm64`, `android-arm64`, `windows-x86_64`,
`windows-arm64`, `macos-arm64`, `macos-x86_64` (macOS targets need a macOS
host). Groups: `linux`, `android`, `windows`, `macos`, `all`.

It runs on a **Linux** or **macOS** host and only sets up the toolchains for the
platforms you select. On Linux (e.g. Arch) it needs:

```bash
sudo pacman -S --needed base-devel rustup unzip && rustup default stable
sudo pacman -S zig              # for linux-* targets (via cargo-zigbuild)
sudo pacman -S mingw-w64-gcc    # for windows-x86_64
```

Cross toolchains are installed to a **persistent** location and reused — the
Android NDK in `~/android-ndk-r26d` and llvm-mingw (for `windows-arm64`, ~80 MB)
in `~/cross-toolchains`, both downloaded automatically; on macOS also the musl
toolchains in `~/cross-toolchains` and mingw-w64 via Homebrew. The large
downloads happen **at most once**; re-running after a bug fix only recompiles
(nothing is re-downloaded). Override the locations with `WHISPR_CROSS_DIR` /
`ANDROID_NDK_HOME` if you prefer.

Binaries are written to `dist/` (git-ignored). Attach the ones you want to
share to a GitHub Release rather than committing them.



### Uninstall

```bash
cargo uninstall whispr
```


## Usage

### Interactive chat (default)

```bash
whispr                       # opens the interactive chat
whispr --mode internet chat  # pre-selects Internet in the menu
whispr --mode internet --net tor chat   # pre-selects Internet + Tor
```

On launch you first get an **in-app mode selector** — pick your transport with
the arrow keys and Enter:

```
How do you want to connect?
  > Wi-Fi / Local network   (LAN — offline, auto-discovery)
    Internet                (peer-to-peer over the internet)
    Bluetooth               (BLE — experimental, not usable yet)
```

Choosing **Internet** then asks for the privacy routing (**Direct**, **Tor**, or
a custom **Proxy** you type in). Any `--mode`/`--net` flags just pre-select the
menu, so pressing Enter accepts them. Press **Esc** to go back a step (or quit
from the first screen).

Then you're in the chat:

- The left **peers** sidebar lists both connected peers (● online) and ones
  discovered on the network (○ found). **Tab** cycles through them.
- To connect to a discovered (○) peer, select it and press **Enter** on an empty
  input line. Once connected it turns into ● and you can message it.
- Type a message and press **Enter** to send it to the selected peer; with no
  connected peer selected it broadcasts to everyone.
- **Up/Down** (or **PageUp/PageDown**) scroll the message log; **End** jumps back
  to following the newest messages.
- `/help` lists commands: `/connect host:port`, `/join <id|name>`,
  `/find <public-key>`, `/discover`, `/peers`, `/whoami`, `/quit`.
- **Esc** or **Ctrl-C** quits.

> **Bluetooth is not usable yet.** Selecting it shows an explanation and returns
> you to the menu. Two reasons: the cross-platform Rust BLE library is
> scan/central-only (it can't act as the GATT *peripheral* two peers need to
> exchange data), and on macOS the OS kills command-line tools that touch
> Bluetooth without a signed `.app` bundle + entitlement. Use **Wi-Fi/LAN** or
> **Internet** to chat. (Developers can still enable BLE *scanning* with
> `--features bluetooth`.)

### Scriptable CLI

```bash
whispr id                                   # show your fingerprint + public key
whispr keygen --force                       # roll a new identity
whispr --mode lan discover --secs 5         # list peers on the local network
whispr --mode lan -p 4001 listen            # headless node; prints messages
whispr --mode lan -p 4002 send 127.0.0.1:4001 "hi"   # one-shot send
```

### Common flags (work with any command)

| Flag | Meaning |
|------|---------|
| `--mode lan\|internet\|bluetooth` | Transport to use (default `lan`) |
| `--net direct\|tor\|proxy` | Routing/anonymity for the internet transport |
| `--proxy host:port` | SOCKS5 proxy address (with `--net proxy`) |
| `--name <name>` | Display name to advertise (anonymous handle by default) |
| `-p, --port <port>` | TCP port to listen on (`0` = random) |
| `--directory host:port` | Rendezvous directory server (internet transport only) |

### Staying reachable when your IP changes (directory server)

Over the internet, a peer's IP changes (mobile, DHCP, NAT), so a raw address
goes stale. The durable identifier is your **public key**, which never changes —
you just need a way to resolve *key → current address*. whispr ships an optional,
**untrusted** rendezvous directory for exactly this:

```bash
# 1. Run a directory somewhere reachable (or use someone else's).
whispr directory --bind 0.0.0.0:6060

# 2. Come online in internet mode with a fixed port; you auto-register.
whispr --mode internet -p 5001 --directory dir.example:6060 chat

# 3. Others reach you by KEY, not IP — the address is looked up for them.
whispr --mode internet --directory dir.example:6060 find <public-key>
whispr --mode internet -p 5002 --directory dir.example:6060 reach <public-key> "hi"
# ...or inside the TUI:  /find <public-key>
```

Why it's safe even if the directory is malicious or compromised:

- Clients **prove key ownership** to the directory using the same Noise
  handshake (no separate signing keys needed), so nobody can register an address
  under someone else's key.
- Messages never traverse the directory — it only ever stores `key → address`.
- When you dial a looked-up address, the peer-to-peer handshake **re-verifies the
  peer's key and rejects any mismatch**, so a bad address fails cleanly instead of
  connecting you to an impostor.
- Lookups use a throwaway handshake key, so they aren't linked to your identity.

The worst a rogue directory can do is refuse to answer (denial of service) or
learn who is online and their addresses (metadata). For location privacy,
combine it with `--net tor`, or publish a `.onion` as your address.

## Roadmap

- Directory: publish `.onion` addresses (not raw IPs) and add optional relaying
  for peers behind strict NAT.
- Bluetooth: implement the encrypted GATT data channel (discovery already works).
- Mobile: wrap the core with `uniffi` for a native iOS/Android front-end.
- Offline mesh: multi-hop relaying so LAN/BLE peers forward for each other.
- Persistent contacts and trust-on-first-use fingerprint pinning.

## License

MIT
