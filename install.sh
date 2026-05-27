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
    Linux)  os="unknown-linux-gnu" ;;
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
if [ "$os" = "unknown-linux-gnu" ] && [ "$arch" != "x86_64" ]; then
    echo "No prebuilt binary for linux/$arch." >&2
    echo "Build from source: https://github.com/$REPO" >&2
    exit 1
fi

target="${arch}-${os}"

if [ "$VERSION" = "latest" ]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
        | head -n1)
    if [ -z "$VERSION" ]; then
        echo "Failed to resolve the latest release tag." >&2
        exit 1
    fi
fi

asset="ignis-${VERSION}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/${VERSION}/${asset}"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t ignis-install)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading ${url}"
curl -fsSL "$url" -o "$tmp/ignis.tar.gz"
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
trap 'rm -f "$staged"; rm -rf "$tmp"' EXIT INT TERM
cp "$src" "$staged"
chmod 0755 "$staged"
mv -f "$staged" "$INSTALL_DIR/ignis"

echo "ignis ${VERSION} installed to ${INSTALL_DIR}/ignis"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo
        echo "Add ${INSTALL_DIR} to your PATH, e.g.:"
        echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile"
        echo "Then restart your shell."
        ;;
esac
