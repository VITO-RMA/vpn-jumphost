#!/bin/bash
# build-deb.sh — Stage and assemble the .deb package for vpn-jumphost.
#
# Usage:
#   ./build-deb.sh                        # uses target/release/jumphost
#   ./build-deb.sh /path/to/jumphost      # use a specific binary
#
# Produces: vpn-jumphost_<version>_amd64.deb in the current directory.
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
CONTROL_VERSION="$(sed -n 's/^Version: //p' "$REPO_ROOT/contrib/debian/control" | head -1)"

if [ -z "$VERSION" ]; then
    echo "error: could not read version from Cargo.toml" >&2
    exit 1
fi
if [ "$VERSION" != "$CONTROL_VERSION" ]; then
    echo "error: Cargo.toml version ($VERSION) != debian/control version ($CONTROL_VERSION)" >&2
    echo "       run 'just bump_version $VERSION' to sync them" >&2
    exit 1
fi

DEB_NAME="vpn-jumphost_${VERSION}_amd64.deb"
echo "Building $DEB_NAME (v$VERSION)"

# ── Staging area ────────────────────────────────────────────────────
PKG_ROOT="$(mktemp -d)"
trap 'rm -rf "$PKG_ROOT"' EXIT
chmod 755 "$PKG_ROOT"

install -d "$PKG_ROOT/DEBIAN"
install -d "$PKG_ROOT/usr/bin"
install -d "$PKG_ROOT/usr/lib/systemd/user"
install -d "$PKG_ROOT/usr/share/vpn-jumphost"
install -d "$PKG_ROOT/usr/share/doc/vpn-jumphost"

install -m 644 "$REPO_ROOT/contrib/debian/control"    "$PKG_ROOT/DEBIAN/control"
install -m 755 "$REPO_ROOT/contrib/debian/postinst"   "$PKG_ROOT/DEBIAN/postinst"
install -m 755 "$REPO_ROOT/contrib/debian/prerm"      "$PKG_ROOT/DEBIAN/prerm"
install -m 755 "$REPO_ROOT/contrib/debian/postrm"     "$PKG_ROOT/DEBIAN/postrm"

install -m 755 "$BINARY" "$PKG_ROOT/usr/bin/jumphost"

# Shell completions (bash, zsh, fish)
install -d "$PKG_ROOT/usr/share/bash-completion/completions"
install -d "$PKG_ROOT/usr/share/zsh/vendor-completions"
install -d "$PKG_ROOT/usr/share/fish/vendor_completions.d"
"$BINARY" generate-completions bash > "$PKG_ROOT/usr/share/bash-completion/completions/jumphost"
"$BINARY" generate-completions zsh  > "$PKG_ROOT/usr/share/zsh/vendor-completions/_jumphost"
"$BINARY" generate-completions fish > "$PKG_ROOT/usr/share/fish/vendor_completions.d/jumphost.fish"

install -m 644 "$REPO_ROOT/contrib/debian/vpn-jumphost.service" \
    "$PKG_ROOT/usr/lib/systemd/user/vpn-jumphost.service"
install -m 644 "$REPO_ROOT/docs/config.example.toml" \
    "$PKG_ROOT/usr/share/vpn-jumphost/config.example.toml"
install -m 644 "$REPO_ROOT/README.md" \
    "$PKG_ROOT/usr/share/doc/vpn-jumphost/README.md"
install -m 644 "$REPO_ROOT/spec.md" \
    "$PKG_ROOT/usr/share/doc/vpn-jumphost/spec.md"
install -m 644 "$REPO_ROOT/docs/architecture.md" \
    "$PKG_ROOT/usr/share/doc/vpn-jumphost/architecture.md"
install -m 644 "$REPO_ROOT/docs/ssh.md" \
    "$PKG_ROOT/usr/share/doc/vpn-jumphost/ssh.md"
install -m 644 "$REPO_ROOT/contrib/debian/copyright" \
    "$PKG_ROOT/usr/share/doc/vpn-jumphost/copyright"
gzip -9n -c "$REPO_ROOT/contrib/debian/changelog" \
    > "$PKG_ROOT/usr/share/doc/vpn-jumphost/changelog.gz"

# ── Build the .deb ──────────────────────────────────────────────────
dpkg-deb --build "$PKG_ROOT" "$DEB_NAME"

echo ""
echo "Built: $DEB_NAME"
dpkg-deb --info "$DEB_NAME"
dpkg-deb --contents "$DEB_NAME"
