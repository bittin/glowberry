<p align="center">
  <img src="data/GlowBerry.svg" alt="GlowBerry Logo" width="128">
</p>

# GlowBerry

An enhanced background/wallpaper service with live shader support for COSMIC DE.

Disclaimer: This project extends the functionality of cosmic-bg with live shader wallpapers. When set up correctly, cosmic-session will run GlowBerry instead of cosmic-bg.

## Features

- Live GPU-rendered shader wallpapers (WGSL)
- Static image wallpapers and solid colors
- Per-display configuration
- Power saving options (pause/reduce FPS on battery)
- Settings application for easy configuration

## Installation

Build and install with [just](https://github.com/casey/just):

```sh
just
sudo just install
```

### Dependencies

- just
- cargo / rustc (install from https://rustup.rs/)
- libwayland-dev
- libxkbcommon-dev
- mold
- pkg-config

## Enabling GlowBerry

GlowBerry works by intercepting cosmic-session's call to `cosmic-bg`. This is done by creating a symlink at `/usr/local/bin/cosmic-bg` that points to `/usr/bin/glowberry`. Since `/usr/local/bin` is searched before `/usr/bin` in PATH, cosmic-session will run GlowBerry instead.

> [!IMPORTANT]
> For this to work, `/usr/local/bin` must appear before `/usr/bin` in your PATH. You can verify this by running:
> ```sh
> echo $PATH | tr ':' '\n' | grep -n bin
> ```

### Using the switch script

Enable GlowBerry (you may need to restart for this to take effect):
```sh
glowberry-switch enable
```

Disable GlowBerry (restore original cosmic-bg):
```sh
glowberry-switch disable
```

Check current status:
```sh
glowberry-switch status
```

### Using the settings app

You can also enable/disable GlowBerry from the settings application (`glowberry-settings`). Open the settings drawer and toggle "Use GlowBerry as default". You may need to restart to clean up old cosmic-bg and use GlowBerry properly.

### Manual setup

If you prefer to set it up manually:

```sh
# Enable GlowBerry
sudo ln -sf /usr/bin/glowberry /usr/local/bin/cosmic-bg
pkill cosmic-bg  # Restart the service

# Disable GlowBerry
sudo rm /usr/local/bin/cosmic-bg
pkill glowberry  # Restart the service
```

## Adding Shaders

Shader wallpapers are WGSL files placed in `/usr/share/glowberry/shaders/`. Example shaders are included in the `examples/` directory.

To install the example shaders (they are installed by default when running `sudo just install`):
```sh
sudo cp examples/*.wgsl /usr/share/glowberry/shaders/
```

## Uninstall

```sh
sudo just uninstall
```

Make sure to disable GlowBerry first (`glowberry-switch disable`) to restore the original cosmic-bg.


## Why GlowBerry?

With the right shader, your desktop can be a glowing berry.
