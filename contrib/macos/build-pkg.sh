#!/bin/bash
# build-pkg.sh — Assemble a macOS installer package (.pkg) for vpn-jumphost.
#
# Usage:
#   ./build-pkg.sh                   # uses target/release/jumphost
#   ./build-pkg.sh /path/to/jumphost # use a specific binary
#
# Produces: vpn-jumphost-<version>-<arch>.pkg in the current directory.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

BINARY="${1:-$REPO_ROOT/target/release/jumphost}"
if [ ! -x "$BINARY" ]; then
    echo "error: binary not found at $BINARY" >&2
    echo "       run 'cargo build --release' first, or pass the path as \$1" >&2
    exit 1
fi

# ── Metadata ────────────────────────────────────────────────────────
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -1)"
ARCH="$(uname -m)"  # arm64 or x86_64
IDENTIFIER="sas.vpn-jumphost"
PKG_NAME="vpn-jumphost-${VERSION}-${ARCH}.pkg"

echo "Building $PKG_NAME (v$VERSION, $ARCH)"

# ── Staging area ────────────────────────────────────────────────────
STAGE="$(mktemp -d)"
SCRIPTS="$(mktemp -d)"
trap 'rm -rf "$STAGE" "$SCRIPTS"' EXIT

# /usr/local/bin/jumphost
install -d "$STAGE/usr/local/bin"
install -m 755 "$BINARY" "$STAGE/usr/local/bin/jumphost"

# /usr/local/share/vpn-jumphost/ (launchd plist + example config)
install -d "$STAGE/usr/local/share/vpn-jumphost"
install -m 644 "$SCRIPT_DIR/sas.vpn-jumphost.plist" \
    "$STAGE/usr/local/share/vpn-jumphost/sas.vpn-jumphost.plist"
install -m 644 "$REPO_ROOT/docs/config.example.toml" \
    "$STAGE/usr/local/share/vpn-jumphost/config.example.toml"

# /Applications/Jumphost.app (minimal bundle for Notification Center)
APP="$STAGE/Applications/Jumphost.app/Contents"
install -d "$APP/MacOS"
# Symlink to the installed binary so the .app stays in sync with updates.
ln -s /usr/local/bin/jumphost "$APP/MacOS/jumphost"
install -m 644 "$SCRIPT_DIR/Info.plist" "$APP/Info.plist"

# ── Installer scripts ──────────────────────────────────────────────
install -m 755 "$SCRIPT_DIR/preinstall"  "$SCRIPTS/preinstall"
install -m 755 "$SCRIPT_DIR/postinstall" "$SCRIPTS/postinstall"

# ── Build the .pkg ──────────────────────────────────────────────────
pkgbuild \
    --root "$STAGE" \
    --scripts "$SCRIPTS" \
    --identifier "$IDENTIFIER" \
    --version "$VERSION" \
    --install-location / \
    "$PKG_NAME"

echo ""
echo "Built: $PKG_NAME"
echo ""
echo "Install with:"
echo "  sudo installer -pkg $PKG_NAME -target /"
echo ""
echo "Or double-click the .pkg in Finder."
