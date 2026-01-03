# systemd-swap

Smart dynamic swap management for Linux, written in Rust.

## Features

- **Auto-detection**: Automatically chooses best swap strategy for your system
- **btrfs optimized**: Uses zswap + swap files on btrfs
- **Universal fallback**: Uses zram on non-btrfs systems
- **Low memory**: ~800 KB binary (vs ~10 MB Python version)

## How It Works

| Filesystem | Strategy | Description |
|------------|----------|-------------|
| **btrfs** | zswap + swapfc | Compressed RAM cache with dynamic swap files |
| **other** | zram only | Compressed RAM disk |

### Why This Choice?

**btrfs systems**: zswap compresses pages in RAM before writing to disk. Combined with dynamic swap files (swapfc), this provides efficient memory management with disk backing when needed.

**non-btrfs systems**: zram creates a compressed swap device entirely in RAM. This is ideal when swap files aren't supported or practical.

## Installation

```bash
# Arch Linux / BigLinux
cd pkgbuild
makepkg -si
```

## Usage

```bash
# Enable and start
sudo systemctl enable --now systemd-swap

# Check status
systemd-swap status

# View compression algorithms
systemd-swap compression
```

## Configuration

Edit `/etc/systemd/swap.conf`:

```ini
# Swap mode (default: auto)
# auto         - Auto-detect filesystem
# zswap+swapfc - Force zswap with swap files
# zram         - Force zram only  
# manual       - Use explicit settings
swap_mode=auto

# Zswap settings (for btrfs mode)
zswap_compressor=zstd
zswap_max_pool_percent=35
zswap_zpool=zsmalloc

# Zram settings (for non-btrfs mode)
zram_size=$RAM_SIZE
zram_alg=zstd
zram_prio=32767

# SwapFC settings (for btrfs mode)
swapfc_chunk_size=512M
swapfc_max_count=32
swapfc_path=/swapfc/swapfile  # Can be on different partition
```

## Custom Swap Location

You can create swap files on a different partition by setting `swapfc_path`:

```ini
# Use swap on separate btrfs partition
swapfc_path=/mnt/swap-drive/swapfile
```

The path must be on a btrfs filesystem.

## File Locations

| Path | Description |
|------|-------------|
| `/usr/bin/systemd-swap` | Main binary |
| `/etc/systemd/swap.conf` | User configuration |
| `/usr/share/systemd-swap/swap-default.conf` | Default configuration |
| `/run/systemd/swap/` | Runtime data |

## License

GPL-3.0-or-later
