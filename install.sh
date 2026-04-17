#!/bin/sh
set -e

# Qartez MCP - zero-dependency installer
# Works on macOS (arm64/x86_64) and Linux (amd64/arm64/riscv64)
# Only needs: curl or wget
#
# Usage:
#   curl -sSfL https://qartez.dev/install | sh
#
# Or from a checked-out repo:
#   ./install.sh
#
# In curl|sh mode, the script downloads the latest source tarball into a
# temp directory and builds from there.

QARTEZ_REPO="kuberstar/qartez-mcp"
QARTEZ_BRANCH="main"
INSTALL_DIR="${HOME}/.local/bin"
SCRIPT_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || SCRIPT_DIR=""

if [ -t 1 ]; then
    GREEN='\033[0;32m'; BLUE='\033[1;34m'; RED='\033[1;31m'; YELLOW='\033[1;33m'; NC='\033[0m'
else
    GREEN=''; BLUE=''; RED=''; YELLOW=''; NC=''
fi

info()  { printf "${BLUE}==>${NC} %s\n" "$1"; }
ok()    { printf "${GREEN}[+]${NC} %s\n" "$1"; }
warn()  { printf "${YELLOW}[!]${NC} %s\n" "$1"; }
err()   { printf "${RED}[!]${NC} %s\n" "$1" >&2; }

# --- Preflight checks ---
if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    err "Neither curl nor wget found. Install one of them first."
    exit 1
fi

if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
    err "No C compiler found (cc, gcc, or clang)."
    err "Rust needs a linker to build. Install one first:"
    case "$(uname)" in
        Darwin) err "  xcode-select --install" ;;
        *)
            if command -v apt-get >/dev/null 2>&1; then
                err "  sudo apt-get install build-essential"
            elif command -v dnf >/dev/null 2>&1; then
                err "  sudo dnf install gcc"
            elif command -v pacman >/dev/null 2>&1; then
                err "  sudo pacman -S base-devel"
            elif command -v apk >/dev/null 2>&1; then
                err "  sudo apk add build-base"
            else
                err "  Install gcc or clang via your package manager"
            fi
            ;;
    esac
    exit 1
fi

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -sSfL -o "$2" "$1"
    else
        wget -qO "$2" "$1"
    fi
}

# --- Source acquisition (curl|sh mode) ---
# When invoked via `curl ... | sh`, $0 is "sh" and SCRIPT_DIR has no Cargo.toml.
# Download the source tarball into a temp dir and build from there.
if [ -z "$SCRIPT_DIR" ] || [ ! -f "${SCRIPT_DIR}/Cargo.toml" ]; then
    if ! command -v tar >/dev/null 2>&1; then
        err "tar not found - required to extract source tarball."
        exit 1
    fi
    info "Source not found locally - downloading from github.com/${QARTEZ_REPO}..."
    QARTEZ_TMPDIR="$(mktemp -d)"
    trap 'rm -rf "$QARTEZ_TMPDIR"' EXIT INT TERM
    download "https://codeload.github.com/${QARTEZ_REPO}/tar.gz/refs/heads/${QARTEZ_BRANCH}" "${QARTEZ_TMPDIR}/qartez.tar.gz"
    tar -xzf "${QARTEZ_TMPDIR}/qartez.tar.gz" -C "$QARTEZ_TMPDIR"
    SCRIPT_DIR="${QARTEZ_TMPDIR}/qartez-mcp-${QARTEZ_BRANCH}"
    if [ ! -f "${SCRIPT_DIR}/Cargo.toml" ]; then
        err "Tarball layout unexpected: ${SCRIPT_DIR}/Cargo.toml not found"
        exit 1
    fi
    ok "Source extracted to ${SCRIPT_DIR}"
fi

# --- Rust ---
# Must match `rust-version` in Cargo.toml. Edition 2024 needs >= 1.85.
RUST_MIN="1.88.0"

if command -v cargo >/dev/null 2>&1; then
    CARGO="$(command -v cargo)"
elif [ -x "${HOME}/.cargo/bin/cargo" ]; then
    CARGO="${HOME}/.cargo/bin/cargo"
else
    info "Rust not found. Installing via rustup..."
    RUSTUP_INIT="$(mktemp)"
    trap 'rm -f "$RUSTUP_INIT"' EXIT
    download https://sh.rustup.rs "$RUSTUP_INIT"
    sh "$RUSTUP_INIT" -y
    rm -f "$RUSTUP_INIT"
    trap - EXIT
    CARGO="${HOME}/.cargo/bin/cargo"
    if ! [ -x "$CARGO" ]; then
        err "cargo not found at $CARGO after rustup install."
        exit 1
    fi
    ok "Rust installed."
fi

# Version check: catch old rustc before cargo emits cryptic feature-gate errors.
# `rustc --version` output: "rustc 1.88.0 (abc 2025-06-26)"
RUSTC_BIN="$(dirname "$CARGO")/rustc"
[ -x "$RUSTC_BIN" ] || RUSTC_BIN="rustc"
if command -v "$RUSTC_BIN" >/dev/null 2>&1; then
    RUSTC_VER="$("$RUSTC_BIN" --version 2>/dev/null | awk '{print $2}' | cut -d- -f1)"
else
    RUSTC_VER=""
fi

if [ -n "$RUSTC_VER" ]; then
    OLDEST="$(printf '%s\n%s\n' "$RUST_MIN" "$RUSTC_VER" | sort -V | head -n 1)"
    if [ "$OLDEST" != "$RUST_MIN" ]; then
        warn "Rust ${RUSTC_VER} is older than the required ${RUST_MIN}."
        if command -v rustup >/dev/null 2>&1; then
            info "Updating Rust toolchain via rustup..."
            rustup update stable
            rustup default stable >/dev/null 2>&1 || true
            RUSTC_VER_NEW="$("$RUSTC_BIN" --version 2>/dev/null | awk '{print $2}' | cut -d- -f1)"
            OLDEST_NEW="$(printf '%s\n%s\n' "$RUST_MIN" "$RUSTC_VER_NEW" | sort -V | head -n 1)"
            if [ "$OLDEST_NEW" != "$RUST_MIN" ]; then
                err "Rust is still ${RUSTC_VER_NEW} after update. Minimum required: ${RUST_MIN}."
                err "Your stable channel may be pinned. Try: rustup default stable && rustup update"
                exit 1
            fi
            ok "Rust updated to ${RUSTC_VER_NEW}."
        else
            err "Rust ${RUSTC_VER} is too old. qartez-mcp requires >= ${RUST_MIN}."
            err "Your rustc was not installed via rustup, so we cannot auto-update it."
            err "Options:"
            err "  1. Install rustup and retry:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
            err "  2. Upgrade Rust via your OS package manager to >= ${RUST_MIN}"
            exit 1
        fi
    fi
fi

# --- Build ---
cd "$SCRIPT_DIR"
info "Building release binaries (this may take a few minutes on first run)..."
"$CARGO" build --release

# --- Install ---
TARGET_DIR="${CARGO_TARGET_DIR:-${SCRIPT_DIR}/target}"
mkdir -p "$INSTALL_DIR"
for bin in qartez qartez-guard qartez-setup; do
    if ! [ -f "${TARGET_DIR}/release/${bin}" ]; then
        err "Binary not found: ${TARGET_DIR}/release/${bin}"
        exit 1
    fi
    # Atomic install: copy to .new, then rename. mv replaces the inode so a
    # running process keeps the old binary mapped via its open fd while new
    # invocations get the fresh one - avoids ETXTBSY and corrupted overwrites.
    cp "${TARGET_DIR}/release/${bin}" "${INSTALL_DIR}/${bin}.new"
    if [ "$(uname)" = "Darwin" ]; then
        codesign -s - -f "${INSTALL_DIR}/${bin}.new" 2>/dev/null || true
    fi
    mv -f "${INSTALL_DIR}/${bin}.new" "${INSTALL_DIR}/${bin}"
    SIZE=$(wc -c < "${TARGET_DIR}/release/${bin}" | awk '{printf "%.1f MB", $1/1048576}')
    ok "Installed: ${INSTALL_DIR}/${bin} (${SIZE})"
done
ln -sf qartez "${INSTALL_DIR}/qartez-mcp"
ok "Symlink: ${INSTALL_DIR}/qartez-mcp -> qartez"

# --- Configure IDEs ---
case "${1:-}" in
    --interactive)
        info "Launching interactive IDE setup..."
        "${INSTALL_DIR}/qartez-setup"
        ;;
    --skip-setup)
        info "Skipping IDE setup (--skip-setup)."
        ;;
    *)
        info "Configuring all detected IDEs..."
        "${INSTALL_DIR}/qartez-setup" --yes
        ;;
esac

ok "Deploy complete. Restart your IDEs to pick up MCP changes."

# --- PATH check ---
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        warn "${INSTALL_DIR} is not on your PATH."
        SHELL_NAME="$(basename "${SHELL:-/bin/sh}")"
        case "$SHELL_NAME" in
            zsh)  PROFILE="\$HOME/.zshrc" ;;
            bash) PROFILE="\$HOME/.bashrc" ;;
            fish) PROFILE="\$HOME/.config/fish/config.fish" ;;
            *)    PROFILE="\$HOME/.profile" ;;
        esac
        warn "Add to ${PROFILE}:"
        warn "  export PATH=\"\$HOME/.local/bin:\$PATH\""
        ;;
esac
