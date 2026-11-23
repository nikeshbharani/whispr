#!/usr/bin/env bash
#
# Cross-platform release builds for whispr, with per-platform selection.
# Works on a Linux host (e.g. Arch) and on a macOS host.
#
# Cross toolchains are installed to PERSISTENT locations and reused, so the
# heavy downloads (Android NDK, musl toolchains) happen at most ONCE.
#
# Usage:
#   scripts/build-release.sh                       # interactive menu
#   scripts/build-release.sh linux android         # pick platforms / groups
#   scripts/build-release.sh linux-x86_64 windows-arm64
#   scripts/build-release.sh all                   # everything this host can build
#   scripts/build-release.sh --list                # show platforms
#
# Platforms:  linux-x86_64  linux-arm64  android-arm64  windows-x86_64
#             windows-arm64 macos-arm64   macos-x86_64  (macos-*: macOS host only)
# Groups:     linux  android  windows  macos  all
#
# Env overrides:
#   WHISPR_CROSS_DIR   cross toolchains (llvm-mingw; musl on macOS)  default ~/cross-toolchains
#   ANDROID_NDK_HOME   Android NDK                         default ~/android-ndk-r26d
#   SKIP_ANDROID=1     drop android from the selection

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
source "$HOME/.cargo/env" 2>/dev/null || true

HOST_OS="$(uname -s)"   # Linux | Darwin
ALL_PLATFORMS=(linux-x86_64 linux-arm64 android-arm64 windows-x86_64 windows-arm64 macos-arm64 macos-x86_64)

log()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m!!  %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31mxx  %s\033[0m\n' "$*" >&2; exit 1; }

usage() { sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; }

host_can_build() { # <platform>
  case "$1" in
    macos-*) [ "$HOST_OS" = "Darwin" ] ;;
    *) true ;;
  esac
}

# --- Parse the selection ----------------------------------------------------
expand() { # <platform-or-group> -> platforms
  case "$1" in
    all)     for p in "${ALL_PLATFORMS[@]}"; do host_can_build "$p" && echo "$p"; done ;;
    linux)   echo linux-x86_64; echo linux-arm64 ;;
    android) echo android-arm64 ;;
    windows) echo windows-x86_64; echo windows-arm64 ;;
    macos)   echo macos-arm64; echo macos-x86_64 ;;
    linux-x86_64|linux-arm64|android-arm64|windows-x86_64|windows-arm64|macos-arm64|macos-x86_64) echo "$1" ;;
    *) die "Unknown platform '$1' (see --list)" ;;
  esac
}

menu() {
  local avail=() i=1
  for p in "${ALL_PLATFORMS[@]}"; do host_can_build "$p" && avail+=("$p"); done
  echo "Which platforms do you want to build? (space-separated numbers, or 'a' for all)" >&2
  for p in "${avail[@]}"; do printf '  %d) %s\n' "$i" "$p" >&2; i=$((i + 1)); done
  printf '> ' >&2
  local reply; read -r reply
  [ -z "$reply" ] && die "Nothing selected"
  if [ "$reply" = "a" ]; then printf '%s\n' "${avail[@]}"; return; fi
  for n in $reply; do
    [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -ge 1 ] && [ "$n" -le "${#avail[@]}" ] \
      || die "Invalid choice '$n'"
    echo "${avail[$((n - 1))]}"
  done
}

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  --list)
    for p in "${ALL_PLATFORMS[@]}"; do
      if host_can_build "$p"; then echo "  $p"; else echo "  $p   (needs a macOS host)"; fi
    done
    exit 0 ;;
esac

SELECTED=()
if [ $# -eq 0 ]; then
  [ -t 0 ] || die "No platforms given (non-interactive). Pass e.g. 'linux android' or 'all'."
  mapfile -t SELECTED < <(menu)
else
  for arg in "$@"; do
    expand "$arg" >/dev/null   # validate in this shell so an error exits the script
    mapfile -t -O "${#SELECTED[@]}" SELECTED < <(expand "$arg")
  done
fi
# de-duplicate, keep order; apply SKIP_ANDROID; reject what this host can't do
declare -A seen=(); PLATFORMS=()
for p in "${SELECTED[@]}"; do
  [ -n "${seen[$p]:-}" ] && continue; seen[$p]=1
  [ "$p" = android-arm64 ] && [ "${SKIP_ANDROID:-0}" = "1" ] && { warn "SKIP_ANDROID=1 — skipping $p"; continue; }
  host_can_build "$p" || die "$p can only be built on a macOS host"
  PLATFORMS+=("$p")
done
[ "${#PLATFORMS[@]}" -gt 0 ] || die "Nothing to build"
log "Building: ${PLATFORMS[*]}"

want() { [[ " ${PLATFORMS[*]} " == *" $1 "* ]]; }

# --- Rust toolchain ---------------------------------------------------------
if ! command -v rustup >/dev/null; then
  if [ "$HOST_OS" = "Linux" ] && command -v pacman >/dev/null; then
    die "rustup not found. Install it with:
      sudo pacman -S --needed base-devel rustup && rustup default stable"
  fi
  die "rustup not found. Install it from https://rustup.rs"
fi

target_of() {
  case "$1" in
    linux-x86_64)   echo x86_64-unknown-linux-musl ;;
    linux-arm64)    echo aarch64-unknown-linux-musl ;;
    android-arm64)  echo aarch64-linux-android ;;
    windows-x86_64) echo x86_64-pc-windows-gnu ;;
    windows-arm64)  echo aarch64-pc-windows-gnullvm ;;
    macos-arm64)    echo aarch64-apple-darwin ;;
    macos-x86_64)   echo x86_64-apple-darwin ;;
  esac
}
log "Ensuring Rust targets"
for p in "${PLATFORMS[@]}"; do rustup target add "$(target_of "$p")" >/dev/null; done

# Ensure a system command exists. If it's missing and we're on an Arch-based
# system in an interactive terminal, offer to install its package with pacman
# (this needs sudo, so we always ask first). Otherwise print how to install it.
need_cmd() { # <command> <pacman package>
  local cmd="$1" pkg="$2"
  command -v "$cmd" >/dev/null && return 0
  if command -v pacman >/dev/null && [ -t 0 ]; then
    printf "\n'%s' is missing. Install it now with 'sudo pacman -S --needed %s'? [Y/n] " "$cmd" "$pkg" >&2
    local reply; read -r reply
    if [[ -z "$reply" || "$reply" =~ ^[Yy] ]]; then
      sudo pacman -S --needed "$pkg"
      command -v "$cmd" >/dev/null && return 0
      die "'$cmd' still not found after installing $pkg"
    fi
  fi
  if command -v pacman >/dev/null; then
    die "'$cmd' not found. Install it with: sudo pacman -S --needed $pkg"
  fi
  die "'$cmd' not found. Install the package that provides it (Arch: $pkg) with your package manager"
}

# --- Per-host toolchain setup (only for what was selected) ------------------
LINUX_BUILDER="cargo build"   # how Linux musl targets are built on this host
# Windows on ARM: GNU mingw can't target it, so use llvm-mingw (an LLVM-based
# mingw toolchain that ships the libunwind Rust needs). Downloaded once (~80 MB).
CROSS_DIR="${WHISPR_CROSS_DIR:-$HOME/cross-toolchains}"
if want windows-arm64; then
  LLVM_MINGW_VERSION="20261117"
  case "$HOST_OS-$(uname -m)" in
    Linux-x86_64)  LLVM_MINGW_HOST="ubuntu-22.04-x86_64" ;;
    Linux-aarch64) LLVM_MINGW_HOST="ubuntu-22.04-aarch64" ;;
    Darwin-*)      LLVM_MINGW_HOST="macos-universal" ;;
    *) die "windows-arm64: no llvm-mingw build for $HOST_OS-$(uname -m)" ;;
  esac
  LLVM_MINGW_NAME="llvm-mingw-$LLVM_MINGW_VERSION-ucrt-$LLVM_MINGW_HOST"
  LLVM_MINGW_DIR="$CROSS_DIR/$LLVM_MINGW_NAME"
  if [ ! -x "$LLVM_MINGW_DIR/bin/aarch64-w64-mingw32-clang" ]; then
    log "Installing llvm-mingw $LLVM_MINGW_VERSION (one-time, ~80 MB) -> $LLVM_MINGW_DIR"
    mkdir -p "$CROSS_DIR"
    curl -fL -o "$CROSS_DIR/$LLVM_MINGW_NAME.tar.xz" \
      "https://github.com/mstorsjo/llvm-mingw/releases/download/$LLVM_MINGW_VERSION/$LLVM_MINGW_NAME.tar.xz"
    tar xJf "$CROSS_DIR/$LLVM_MINGW_NAME.tar.xz" -C "$CROSS_DIR"
    rm -f "$CROSS_DIR/$LLVM_MINGW_NAME.tar.xz"
  fi
  # Point at the linker by absolute path. Do NOT prepend llvm-mingw to PATH: it
  # also ships an `x86_64-w64-mingw32-gcc` (really clang, without libgcc) that
  # would shadow the real mingw-w64 GCC and break the windows-x86_64 build.
  export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER="$LLVM_MINGW_DIR/bin/aarch64-w64-mingw32-clang"
fi
if [ "$HOST_OS" = "Linux" ]; then
  if want linux-x86_64 || want linux-arm64; then
    # zig is a self-contained cross C toolchain; cargo-zigbuild wires it up and
    # overrides the macOS-only linker names in .cargo/config.toml.
    need_cmd zig zig
    command -v cargo-zigbuild >/dev/null || { log "Installing cargo-zigbuild"; cargo install cargo-zigbuild; }
    LINUX_BUILDER="cargo zigbuild"
  fi
  if want windows-x86_64; then
    need_cmd x86_64-w64-mingw32-gcc mingw-w64-gcc
  fi
  NDK_HOST_TAG="linux"
else # Darwin
  if want windows-x86_64 && ! command -v x86_64-w64-mingw32-gcc >/dev/null; then
    log "Installing mingw-w64 (Windows linker)"
    brew install mingw-w64
  fi
  if want linux-x86_64 || want linux-arm64; then
    MUSL_BASE="https://github.com/messense/homebrew-macos-cross-toolchains/releases/download/v15.2.0"
    MUSL_HOST="$( [ "$(uname -m)" = arm64 ] && echo aarch64-darwin || echo x86_64-darwin )"
    for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
      if [ ! -x "$CROSS_DIR/$t/bin/$t-gcc" ]; then
        log "Installing musl toolchain (one-time): $t"
        mkdir -p "$CROSS_DIR"
        curl -fL -o "$CROSS_DIR/$t.tar.gz" "$MUSL_BASE/$t-$MUSL_HOST.tar.gz"
        tar xzf "$CROSS_DIR/$t.tar.gz" -C "$CROSS_DIR"
        rm -f "$CROSS_DIR/$t.tar.gz"
      fi
      export PATH="$CROSS_DIR/$t/bin:$PATH"
    done
  fi
  NDK_HOST_TAG="darwin"
fi

# --- Don't leak the build machine's paths into release binaries --------------
# rustc embeds absolute source paths (panic locations, debug info), which would
# expose your username/home directory (e.g. /home/you/... or /Users/you/...) to
# anyone who runs `strings` on a distributed binary. Rewrite them to neutral ones.
SYSROOT="$(rustc --print sysroot)"
# rustc lets the LAST matching rule win, so the broad $HOME rule goes first.
REMAP=(
  "--remap-path-prefix=$HOME=/home/user"
  "--remap-path-prefix=$SYSROOT=/rust"
  "--remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"
  "--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
  "--remap-path-prefix=$PROJECT_DIR=/whispr"
)
# cargo reads this (unit-separator-delimited) and it is respected by cargo-ndk
# and cargo-zigbuild too.
CARGO_ENCODED_RUSTFLAGS="$(IFS=$'\x1f'; echo "${REMAP[*]}")"
export CARGO_ENCODED_RUSTFLAGS

# --- Build ------------------------------------------------------------------
mkdir -p dist
build() { # <builder> <rust-target> <dist-name> <src-bin-name>
  local builder="$1" target="$2" out="$3" bin="$4"
  log "Building $target"
  $builder --release --target "$target"
  cp "target/$target/release/$bin" "dist/$out"
}
# Bluetooth is intentionally OFF: BLE messaging isn't implemented yet.
want macos-arm64    && build "cargo build"    aarch64-apple-darwin       whispr-macos-arm64        whispr
want macos-x86_64   && build "cargo build"    x86_64-apple-darwin        whispr-macos-x86_64       whispr
want windows-x86_64 && build "cargo build"    x86_64-pc-windows-gnu      whispr-windows-x86_64.exe whispr.exe
want windows-arm64  && build "cargo build"    aarch64-pc-windows-gnullvm whispr-windows-arm64.exe  whispr.exe
want linux-x86_64   && build "$LINUX_BUILDER" x86_64-unknown-linux-musl  whispr-linux-x86_64       whispr
want linux-arm64    && build "$LINUX_BUILDER" aarch64-unknown-linux-musl whispr-linux-arm64        whispr

# Binaries built with an explicit --target skip rustc's automatic ad-hoc code
# signing, so macOS Gatekeeper kills them on launch. Re-sign them ad-hoc.
if [ "$HOST_OS" = "Darwin" ]; then
  for b in dist/whispr-macos-arm64 dist/whispr-macos-x86_64; do
    [ -f "$b" ] || continue
    xattr -c "$b" 2>/dev/null || true
    codesign --force --sign - "$b" 2>/dev/null || true
  done
fi

# --- Android: install the NDK ONCE (persisted), then build ------------------
if want android-arm64; then
  NDK_VERSION="r26d"
  if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt" ]; then
    NDK_DIR="$ANDROID_NDK_HOME"
  else
    NDK_DIR="$HOME/android-ndk-$NDK_VERSION"
  fi
  if [ ! -d "$NDK_DIR/toolchains/llvm/prebuilt" ]; then
    log "Installing Android NDK $NDK_VERSION (one-time, large download) -> $NDK_DIR"
    if [ "$NDK_HOST_TAG" = "linux" ]; then
      need_cmd unzip unzip
      tmp_zip="$(mktemp --suffix=.zip)"
      tmp_dir="$(mktemp -d)"
      curl -fL -o "$tmp_zip" "https://dl.google.com/android/repository/android-ndk-$NDK_VERSION-linux.zip"
      unzip -q "$tmp_zip" -d "$tmp_dir"
      mkdir -p "$(dirname "$NDK_DIR")"
      mv "$tmp_dir/android-ndk-$NDK_VERSION" "$NDK_DIR"
      rm -rf "$tmp_zip" "$tmp_dir"
    else
      tmp_dmg="$(mktemp -t whispr-ndk).dmg"
      mnt="$(mktemp -d /tmp/whispr-ndk.XXXX)"
      curl -fL -o "$tmp_dmg" "https://dl.google.com/android/repository/android-ndk-$NDK_VERSION-darwin.dmg"
      hdiutil attach "$tmp_dmg" -nobrowse -mountpoint "$mnt" >/dev/null
      src="$(find "$mnt" -type d -path '*/Contents/NDK' | head -1)"
      mkdir -p "$NDK_DIR"
      cp -R "$src/." "$NDK_DIR/"
      hdiutil detach "$mnt" >/dev/null
      rm -f "$tmp_dmg"
    fi
    log "NDK cached at $NDK_DIR (future runs reuse it)"
  fi
  command -v cargo-ndk >/dev/null || { log "Installing cargo-ndk"; cargo install cargo-ndk; }
  log "Building aarch64-linux-android"
  ANDROID_NDK_HOME="$NDK_DIR" cargo ndk -t arm64-v8a -P 24 build --release
  cp target/aarch64-linux-android/release/whispr dist/whispr-android-arm64
fi

chmod +x dist/* 2>/dev/null || true
log "Done. Binaries in dist/:"
ls -1sh dist
