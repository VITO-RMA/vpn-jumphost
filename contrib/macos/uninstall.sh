#!/bin/bash
# uninstall.sh — Remove vpn-jumphost from macOS.
#
# Unloads the launchd agent, removes installed files, and forgets the
# package receipt.  User config at ~/.config/vpn-jumphost/ is left
# intact.
set -euo pipefail

LABEL="sas.vpn-jumphost"
PLIST="$HOME/Library/LaunchAgents/${LABEL}.plist"

echo "Uninstalling vpn-jumphost..."

# ── Stop the launchd agent ──────────────────────────────────────────
if [ -f "$PLIST" ]; then
    launchctl bootout "gui/$(id -u)" "$PLIST" 2>/dev/null || true
    rm -f "$PLIST"
    echo "  Removed launchd agent."
fi

# ── Remove installed files ──────────────────────────────────────────
sudo rm -f  /usr/local/bin/jumphost
sudo rm -rf /usr/local/share/vpn-jumphost
sudo rm -rf /Applications/Jumphost.app
echo "  Removed binary, app bundle, and support files."

# ── Forget the package receipt ──────────────────────────────────────
sudo pkgutil --forget "$LABEL" 2>/dev/null || true
echo "  Forgot package receipt."

echo ""
echo "  Done.  User config remains at:"
echo "    ~/.config/vpn-jumphost/"
echo "  Remove manually if no longer needed."
echo ""
