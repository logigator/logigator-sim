#!/usr/bin/env bash
set -euo pipefail

JUST_VERSION="1.58.0"
WASM_PACK_VERSION="0.15.0"

TARGET="x86_64-unknown-linux-musl"
BIN_DIR="/usr/local/bin"

log() { printf '\n==> %s\n' "$*"; }

# Download a .tar.gz into a fresh temp dir and print that dir.
fetch_tarball() {
    local dir
    dir="$(mktemp -d)"
    curl -fsSL --retry 3 "$1" | tar -xz -C "$dir"
    printf '%s' "$dir"
}

has_version() { command -v "$1" >/dev/null 2>&1 && "$1" --version 2>/dev/null | grep -qF "$2"; }

# --- Rust ------------------------------------------------------------------------------------
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    log "Installing rustup"
    curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain none --no-modify-path
fi
export PATH="$CARGO_HOME/bin:$PATH"

log "Installing Rust stable (+ rustfmt, clippy, rust-analyzer)"
rustup toolchain install stable --profile minimal --component rustfmt,clippy,rust-analyzer,rust-src
rustup default stable

# Expose the rustup proxies on the default PATH so non-login shells find them too.
for tool in "$CARGO_HOME"/bin/*; do
    ln -sf "$tool" "$BIN_DIR/$(basename "$tool")"
done
for rc in "$HOME/.profile" "$HOME/.bashrc"; do
    grep -qF '.cargo/env' "$rc" 2>/dev/null || echo '. "$HOME/.cargo/env"' >>"$rc"
done

# --- Standalone tool binaries ----------------------------------------------------------------
if ! has_version just "$JUST_VERSION"; then
    log "Installing just $JUST_VERSION"
    dir="$(fetch_tarball "https://github.com/casey/just/releases/download/$JUST_VERSION/just-$JUST_VERSION-$TARGET.tar.gz")"
    install -m 0755 "$dir/just" "$BIN_DIR/just"
    rm -rf "$dir"
fi

if ! has_version wasm-pack "$WASM_PACK_VERSION"; then
    log "Installing wasm-pack $WASM_PACK_VERSION"
    dir="$(fetch_tarball "https://github.com/wasm-bindgen/wasm-pack/releases/download/v$WASM_PACK_VERSION/wasm-pack-v$WASM_PACK_VERSION-$TARGET.tar.gz")"
    install -m 0755 "$dir"/wasm-pack-*/wasm-pack "$BIN_DIR/wasm-pack"
    rm -rf "$dir"
fi

log "Toolchain ready"
rustc --version
just --version
wasm-pack --version
