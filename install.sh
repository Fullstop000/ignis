#!/usr/bin/env sh
# Install ignis — downloads the latest release tarball for your platform
# and drops the binary into $IGNIS_INSTALL_DIR (default ~/.ignis/bin).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Fullstop000/ignis/master/install.sh | sh
#   curl -fsSL …/install.sh | IGNIS_VERSION=v0.14.1 sh
#   curl -fsSL …/install.sh | IGNIS_INSTALL_DIR=/usr/local/bin sh
#
# Reinstall is safe — the binary is replaced atomically. Windows is not
# supported here; download from the releases page instead.

set -eu

REPO="Fullstop000/ignis"
INSTALL_DIR="${IGNIS_INSTALL_DIR:-$HOME/.ignis/bin}"
VERSION="${IGNIS_VERSION:-latest}"

uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
    # musl-static on Linux so the binary runs anywhere — including older glibc
    # base images (TB2 sandboxes, slim CI runners, etc.) where the previous
    # `unknown-linux-gnu` build refused to load.
    Linux)  os="unknown-linux-musl" ;;
    Darwin) os="apple-darwin" ;;
    *)
        echo "Unsupported OS: $uname_s." >&2
        echo "Windows users: download from https://github.com/$REPO/releases" >&2
        exit 1
        ;;
esac

case "$uname_m" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *)
        echo "Unsupported architecture: $uname_m" >&2
        exit 1
        ;;
esac

# Releases only ship linux/x86_64 today; refuse other linux arches up front.
if [ "$os" = "unknown-linux-musl" ] && [ "$arch" != "x86_64" ]; then
    echo "No prebuilt binary for linux/$arch." >&2
    echo "Build from source: https://github.com/$REPO" >&2
    exit 1
fi

target="${arch}-${os}"

if [ "$VERSION" = "latest" ]; then
    # Resolve the latest tag via the `/releases/latest` HTML redirect instead
    # of the JSON API, which is rate-limited to 60 req/hr per IP for
    # unauthenticated callers — shared IPs (WSL/corp NAT/CI) hit that wall
    # constantly. The redirect endpoint isn't subject to the same limit and
    # is the standard installer pattern (rustup/starship/etc).
    redirect_url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        "https://github.com/$REPO/releases/latest" 2>/dev/null || true)
    VERSION=${redirect_url##*/}
    if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ]; then
        echo "Failed to resolve the latest release tag." >&2
        echo "Tip: pin a version explicitly, e.g." >&2
        echo "  curl -fsSL .../install.sh | IGNIS_VERSION=vX.Y.Z sh" >&2
        echo "Browse releases: https://github.com/$REPO/releases" >&2
        exit 1
    fi
fi

asset="ignis-${VERSION}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/${VERSION}/${asset}"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t ignis-install)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading ${url}"
# GitHub's release-asset CDN intermittently resets the TLS connection (curl
# exit 35 / SSL_ERROR_SYSCALL), which a single-shot download surfaces as a hard
# failure. Retry a few times before giving up — the next attempt almost always
# succeeds. (The failing command is the `until` condition, so `set -e` is fine.)
attempt=1
until curl -fsSL "$url" -o "$tmp/ignis.tar.gz"; do
    if [ "$attempt" -ge 3 ]; then
        echo "Download failed after 3 attempts: $url" >&2
        echo "GitHub's release CDN may be unreachable from your network." >&2
        echo "Retry, or download manually: https://github.com/$REPO/releases" >&2
        exit 1
    fi
    echo "Download attempt $attempt failed; retrying in 2s..." >&2
    attempt=$((attempt + 1))
    sleep 2
done
tar -xzf "$tmp/ignis.tar.gz" -C "$tmp"

src="$tmp/ignis-${VERSION}-${target}/ignis"
if [ ! -f "$src" ]; then
    echo "Archive layout unexpected: $src missing." >&2
    exit 1
fi

mkdir -p "$INSTALL_DIR"
# Atomic replace: stage in a sibling of the destination, chmod, then mv.
# `install` copies in place and can leave a half-written binary on signal /
# ENOSPC. `mv` within the same directory uses rename(2) — all-or-nothing.
staged="$INSTALL_DIR/.ignis.install.$$"
tui_staged="$HOME/.ignis/.ignis-tui.install.$$"
trap 'rm -f "$staged"; rm -rf "$tui_staged" "$tmp"' EXIT INT TERM
cp "$src" "$staged"
chmod 0755 "$staged"
mv -f "$staged" "$INSTALL_DIR/ignis"

echo "ignis ${VERSION} installed to ${INSTALL_DIR}/ignis"

# Optional Ink frontend: releases bundle `ignis-tui/` (the Node frontend with its
# deps) next to the binary. Lay it down at ~/.ignis/ignis-tui so `ignis` finds it
# regardless of the install dir; `ignis` runs it by default when Node >=18 is on
# PATH and falls back to the built-in TUI otherwise. Older tarballs omit it.
tui_src="$tmp/ignis-${VERSION}-${target}/ignis-tui"
if [ -d "$tui_src" ]; then
    mkdir -p "$HOME/.ignis"
    rm -rf "$tui_staged"
    cp -R "$tui_src" "$tui_staged"
    rm -rf "$HOME/.ignis/ignis-tui"
    mv -f "$tui_staged" "$HOME/.ignis/ignis-tui"
    # ink 5 / react 18 need Node >=18; older or missing Node falls back to the
    # built-in TUI at runtime, so only advertise Ink when the version is right.
    node_major=$(node --version 2>/dev/null | sed 's/^v//; s/\..*//')
    if [ -n "$node_major" ] && [ "$node_major" -ge 18 ] 2>/dev/null; then
        echo "Ink frontend installed (default UI). Set IGNIS_FRONTEND=native for the built-in TUI."
    else
        echo "Ink frontend installed, but Node >=18 was not found — ignis will use the"
        echo "built-in TUI until Node (>=18) is on your PATH."
    fi
fi

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo
        echo "Add ${INSTALL_DIR} to your PATH, e.g.:"
        echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile"
        echo "Then restart your shell."
        ;;
esac
