name := 'glowberry'
settings-name := 'glowberry-settings'
export APPID := 'io.github.hojjatabdollahi.glowberry'
settings-appid := 'io.github.hojjatabdollahi.glowberry-settings'

# Use mold linker if clang and mold exists.
clang-path := `which clang || true`
mold-path := `which mold || true`

linker-arg := if clang-path != '' {
    if mold-path != '' {
        '-C linker=' + clang-path + ' -C link-arg=--ld-path=' + mold-path + ' '
    } else {
        ''
    }
} else {
    ''
}

export RUSTFLAGS := linker-arg + env_var_or_default('RUSTFLAGS', '')

rootdir := ''
prefix := '/usr'


base-dir := absolute_path(clean(rootdir / prefix))

export INSTALL_DIR := base-dir / 'share'

cargo-target-dir := env('CARGO_TARGET_DIR', 'target')
bin-src := cargo-target-dir / 'release' / name
bin-dst := base-dir / 'bin' / name
settings-bin-src := cargo-target-dir / 'release' / settings-name
settings-bin-dst := base-dir / 'bin' / settings-name
shaders-dir := base-dir / 'share' / 'glowberry' / 'shaders'

# Helper script install location
switch-script-dst := base-dir / 'bin' / 'glowberry-switch'

# Settings app data locations
settings-desktop-src := 'apps' / settings-name / 'data' / settings-appid + '.desktop'
settings-desktop-dst := base-dir / 'share' / 'applications' / settings-appid + '.desktop'
settings-icon-src := 'apps' / settings-name / 'data' / 'icons' / settings-appid + '.svg'
settings-icon-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'scalable' / 'apps' / settings-appid + '.svg'
settings-symbolic-src := 'apps' / settings-name / 'data' / 'icons' / settings-appid + '-symbolic.svg'
settings-symbolic-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'symbolic' / 'apps' / settings-appid + '-symbolic.svg'

# Default recipe which runs `just build-release`
default: build-release

# Runs `cargo clean`
clean:
    cargo clean

# `cargo clean` and removes vendored dependencies
clean-dist: clean
    rm -rf .cargo vendor vendor.tar

# Compiles with debug profile
build-debug *args:
    cargo build --workspace {{args}}

# Compiles with release profile
build-release *args: (build-debug '--release' args)

# Compiles release profile with vendored dependencies
build-vendored *args: vendor-extract (build-release '--frozen --offline' args)

# Runs a clippy check
check *args:
    cargo clippy --all-features {{args}} -- -W clippy::pedantic

# Runs a clippy check with JSON message format
check-json: (check '--message-format=json')

# Run with debug logs
run *args:
    env RUST_LOG=debug RUST_BACKTRACE=1 cargo run --release {{args}}

# Run settings app with debug logs
run-settings *args:
    env RUST_LOG=debug RUST_BACKTRACE=1 cargo run --release -p glowberry-settings {{args}}

# Installs all files (daemon + settings app)
install: install-daemon install-settings
    @echo ""
    @echo "=========================================="
    @echo "  GlowBerry installed successfully!"
    @echo "=========================================="
    @echo ""
    @echo "To enable GlowBerry as your background service, run:"
    @echo ""
    @echo "  glowberry-switch enable"
    @echo ""
    @echo "This will create a symlink so cosmic-session runs"
    @echo "GlowBerry instead of the original cosmic-bg."
    @echo ""
    @echo "You can also enable it from glowberry-settings."
    @echo ""

# Installs only the daemon
install-daemon:
    install -Dm0755 {{bin-src}} {{bin-dst}}
    @just data/install
    @just data/icons/install
    # Install bundled shaders for live wallpapers
    install -d {{shaders-dir}}
    install -Dm0644 examples/*.wgsl {{shaders-dir}}/
    # Install the switch helper script
    install -Dm0755 scripts/glowberry-switch {{switch-script-dst}}

# Installs only the settings app
install-settings:
    install -Dm0755 {{settings-bin-src}} {{settings-bin-dst}}
    install -Dm0644 {{settings-desktop-src}} {{settings-desktop-dst}}
    install -Dm0644 {{settings-icon-src}} {{settings-icon-dst}}
    install -Dm0644 {{settings-symbolic-src}} {{settings-symbolic-dst}}

# Uninstalls all installed files
uninstall: _check-glowberry-disabled uninstall-daemon uninstall-settings

# Check if GlowBerry override is disabled before uninstalling
_check-glowberry-disabled:
    #!/usr/bin/env bash
    if [ -L /usr/local/bin/cosmic-bg ]; then
        TARGET=$(readlink /usr/local/bin/cosmic-bg)
        if echo "$TARGET" | grep -q glowberry; then
            echo "=========================================="
            echo "  WARNING: GlowBerry is still enabled!"
            echo "=========================================="
            echo ""
            echo "The symlink at /usr/local/bin/cosmic-bg still points to GlowBerry."
            echo "Please disable it first before uninstalling:"
            echo ""
            echo "  glowberry-switch disable"
            echo ""
            echo "Then run 'sudo just uninstall' again."
            echo ""
            exit 1
        fi
    fi

# Uninstalls only the daemon
uninstall-daemon:
    rm -f {{bin-dst}}
    rm -rf {{shaders-dir}}
    rm -f {{switch-script-dst}}
    @just data/uninstall
    @just data/icons/uninstall

# Uninstalls only the settings app
uninstall-settings:
    rm -f {{settings-bin-dst}}
    rm -f {{settings-desktop-dst}}
    rm -f {{settings-icon-dst}}
    rm -f {{settings-symbolic-dst}}

# Enable GlowBerry as the cosmic-bg replacement via /usr/local/bin
enable-glowberry:
    #!/usr/bin/env bash
    set -e
    
    echo "Setting up GlowBerry as cosmic-bg replacement..."
    
    # Check PATH order - /usr/local/bin should come before /usr/bin
    PATH_ORDER=$(echo "$PATH" | tr ':' '\n' | grep -n -E '^/usr/local/bin$|^/usr/bin$' | head -2)
    LOCAL_POS=$(echo "$PATH_ORDER" | grep '/usr/local/bin' | cut -d: -f1)
    USR_POS=$(echo "$PATH_ORDER" | grep -E '^[0-9]+:/usr/bin$' | cut -d: -f1)
    
    if [ -n "$LOCAL_POS" ] && [ -n "$USR_POS" ]; then
        if [ "$LOCAL_POS" -gt "$USR_POS" ]; then
            echo "WARNING: /usr/local/bin comes AFTER /usr/bin in your PATH!"
            echo "         GlowBerry override may not work correctly."
            echo "         Consider adding 'export PATH=/usr/local/bin:\$PATH' to your shell profile."
            echo ""
        fi
    elif [ -z "$LOCAL_POS" ]; then
        echo "WARNING: /usr/local/bin is not in your PATH!"
        echo "         GlowBerry override may not work correctly."
        echo "         Consider adding 'export PATH=/usr/local/bin:\$PATH' to your shell profile."
        echo ""
    fi
    
    # Create /usr/local/bin if it doesn't exist
    if [ ! -d /usr/local/bin ]; then
        sudo mkdir -p /usr/local/bin
    fi
    
    # Create symlink to glowberry
    sudo ln -sf {{bin-dst}} /usr/local/bin/cosmic-bg
    echo "Created symlink: /usr/local/bin/cosmic-bg -> {{bin-dst}}"
    
    echo ""
    echo "GlowBerry is now active!"
    echo "The original cosmic-bg at /usr/bin/cosmic-bg is unchanged."
    echo ""
    echo "To switch back to original cosmic-bg, run: just disable-glowberry"

# Disable GlowBerry and restore original cosmic-bg
disable-glowberry:
    #!/usr/bin/env bash
    set -e
    
    echo "Disabling GlowBerry override..."
    
    if [ -L /usr/local/bin/cosmic-bg ]; then
        sudo rm /usr/local/bin/cosmic-bg
        echo "Removed /usr/local/bin/cosmic-bg symlink"
        echo ""
        echo "Original cosmic-bg at /usr/bin/cosmic-bg is now active!"
    else
        echo "No GlowBerry override found at /usr/local/bin/cosmic-bg"
    fi

# Check which cosmic-bg is currently active
which-cosmic-bg:
    #!/usr/bin/env bash
    echo "=== cosmic-bg status ==="
    echo ""
    
    # Check PATH order
    echo "PATH order check:"
    PATH_ORDER=$(echo "$PATH" | tr ':' '\n' | grep -n -E '^/usr/local/bin$|^/usr/bin$' | head -2)
    LOCAL_POS=$(echo "$PATH_ORDER" | grep '/usr/local/bin' | cut -d: -f1)
    USR_POS=$(echo "$PATH_ORDER" | grep -E '^[0-9]+:/usr/bin$' | cut -d: -f1)
    
    if [ -n "$LOCAL_POS" ] && [ -n "$USR_POS" ]; then
        if [ "$LOCAL_POS" -lt "$USR_POS" ]; then
            echo "  OK: /usr/local/bin (position $LOCAL_POS) comes before /usr/bin (position $USR_POS)"
        else
            echo "  WARNING: /usr/local/bin (position $LOCAL_POS) comes AFTER /usr/bin (position $USR_POS)"
        fi
    elif [ -z "$LOCAL_POS" ]; then
        echo "  WARNING: /usr/local/bin is not in PATH"
    fi
    
    # Check which binary is in PATH
    echo ""
    WHICH_BG=$(which cosmic-bg 2>/dev/null || echo "not found")
    echo "Active cosmic-bg: $WHICH_BG"
    
    # Check if it's a symlink
    if [ -L "$WHICH_BG" ]; then
        TARGET=$(readlink -f "$WHICH_BG")
        echo "  -> Points to: $TARGET"
    fi
    
    # Check /usr/local/bin override
    echo ""
    echo "/usr/local/bin/cosmic-bg:"
    if [ -e /usr/local/bin/cosmic-bg ]; then
        ls -la /usr/local/bin/cosmic-bg
        if [ -L /usr/local/bin/cosmic-bg ]; then
            echo "  -> $(readlink /usr/local/bin/cosmic-bg)"
        fi
    else
        echo "  Not present (GlowBerry override not active)"
    fi
    
    # Check /usr/bin/cosmic-bg
    echo ""
    echo "/usr/bin/cosmic-bg:"
    if [ -e /usr/bin/cosmic-bg ]; then
        ls -la /usr/bin/cosmic-bg
    else
        echo "  Not found"
    fi



# Update desktop database and icon cache after installation
update-cache:
    #!/usr/bin/env bash
    if command -v update-desktop-database &> /dev/null; then
        sudo update-desktop-database {{base-dir}}/share/applications
    fi
    if command -v gtk-update-icon-cache &> /dev/null; then
        sudo gtk-update-icon-cache -f {{base-dir}}/share/icons/hicolor
    fi
    echo "Application cache updated"

# Vendor dependencies locally
vendor:
    mkdir -p .cargo
    cargo vendor --sync Cargo.toml --sync config/Cargo.toml --sync apps/glowberry-settings/Cargo.toml | head -n -1 > .cargo/config.toml
    echo 'directory = "vendor"' >> .cargo/config.toml
    tar pcf vendor.tar vendor
    rm -rf vendor

# Extracts vendored dependencies
vendor-extract:
    #!/usr/bin/env sh
    rm -rf vendor
    tar pxf vendor.tar
