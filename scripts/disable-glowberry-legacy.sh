#!/usr/bin/env bash
# Disable legacy system-wide GlowBerry override and restore original cosmic-bg
#
# The old installation created a symlink at /usr/local/bin/cosmic-bg
# pointing to /usr/bin/glowberry. This script removes that symlink.

set -e

SYMLINK_PATH="/usr/local/bin/cosmic-bg"

echo "Disabling legacy GlowBerry override..."

# Remove the old symlink if it exists
if [ -L "$SYMLINK_PATH" ]; then
    TARGET=$(readlink "$SYMLINK_PATH")
    echo "Found legacy symlink: $SYMLINK_PATH -> $TARGET"
    sudo rm -f "$SYMLINK_PATH"
    echo "Removed $SYMLINK_PATH"
else
    echo "No legacy symlink found at $SYMLINK_PATH"
fi

# Kill any running cosmic-bg/glowberry processes
echo ""
echo "Killing running background processes..."
pkill -x cosmic-bg 2>/dev/null || true
pkill -x glowberry 2>/dev/null || true

# Verify
echo ""
echo "Verification:"
echo "  which cosmic-bg: $(which cosmic-bg 2>/dev/null || echo 'not found')"
if [ -f /usr/bin/cosmic-bg ]; then
    echo "  /usr/bin/cosmic-bg exists (original)"
fi

echo ""
echo "Legacy override removed."
echo "To fully remove the old system-wide installation, run:"
echo "  sudo just uninstall-legacy"
echo ""
echo "Log out and back in, or the background service will restart automatically."
