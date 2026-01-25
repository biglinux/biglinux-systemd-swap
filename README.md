# systemd-swap

Smart dynamic swap management for Linux, written in Rust.

## Overview

`systemd-swap` automatically configures the best swap strategy for your system:

-   **Auto-detection**: Smartly chooses between Zswap and Zram based on your filesystem.
-   **Zswap + SwapFC**: (Default for Btrfs/Ext4/XFS) Uses compressed RAM cache + dynamic swap files. Most efficient for desktops.
-   **Zram**: (Default for others) Uses compressed RAM block device. Good for systems without disk swap support.
-   **Dynamic Scaling**: Creates swap files on-demand, starting small and growing as needed.

## Installation

### Arch Linux / BigLinux / Manjaro...
```bash
cd pkgbuild
makepkg -si
```

### Manual Build
Requirements: Rust 1.70+, `btrfs-progs`, `util-linux`

```bash
cargo build --release
sudo make install
sudo systemctl enable --now systemd-swap
```

## Usage

Check status:
```bash
systemd-swap status
```

Reload configuration:
```bash
sudo systemctl restart systemd-swap
```

## Configuration

Configuration is located at `/etc/systemd/swap.conf`.

### Common Options

**Change Swap Mode:**
```ini
# Modes: auto, zswap+swapfc, zram, manuall
swap_mode=auto
```

**Customize Zram Size:**
```ini
zram_size=50%
```

**Customize Swap File Location:**
```ini
swapfc_path=/mnt/data/swapfile
```

See `/usr/share/systemd-swap/swap-default.conf` for all available options and defaults.

## Features

-   **Zero Configuration**: Works out of the box for most systems.
-   **Sparse Files**: Swap files use zero disk space until data is actually written (when using Zswap).
-   **MGLRU Support**: Integrates with Multi-Gen LRU (Kernel 6.1+) to prevent thrashing.
-   **Zram Writeback**: Can offload cold pages from Zram to disk.

## License

GPL-3.0-or-later
