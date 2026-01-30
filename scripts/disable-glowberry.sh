#!/usr/bin/env bash
# Disable GlowBerry and restore original cosmic-bg

set -e

echo "Disabling GlowBerry override..."

# Remove the symlink if it exists
if [ -L /usr/local/bin/cosmic-bg ]; then
    sudo rm /usr/local/bin/cosmic-bg
    echo "Removed /usr/local/bin/cosmic-bg symlink"
else
    echo "No symlink found at /usr/local/bin/cosmic-bg"
fi

# Kill any running cosmic-bg/glowberry processes
echo ""
echo "Killing running background processes..."
pkill -f cosmic-bg 2>/dev/null || true

# Verify
echo ""
echo "Verification:"
echo "  which cosmic-bg: $(which cosmic-bg 2>/dev/null || echo 'not found')"
if [ -f /usr/bin/cosmic-bg ]; then
    echo "  /usr/bin/cosmic-bg: $(file /usr/bin/cosmic-bg | cut -d: -f2)"
fi

echo ""
echo "Original cosmic-bg at /usr/bin/cosmic-bg is now active!"
echo "Log out and back in, or run: cosmic-bg &"
