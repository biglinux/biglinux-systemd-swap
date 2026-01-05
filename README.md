# systemd-swap

Smart dynamic swap management for Linux, written in Rust.

## Features

- **Auto-detection**: Automatically chooses optimal swap strategy for your filesystem
- **Multi-filesystem**: Supports btrfs, ext4, and xfs for swap files
- **Zswap + SwapFC**: Compressed RAM cache with dynamic swap files
- **Zram + SwapFC**: Alternative mode with zram as primary swap
- **Zram writeback**: Move idle pages from zram to disk (kernel 5.4+)
- **Fallback support**: Automatically falls back to zram-only if swap files fail
- **Lightweight**: ~250 KB binary vs ~10 MB Python version

## Swap Modes

| Mode | Description | Best For |
|------|-------------|----------|
| `auto` | Auto-detect: btrfs/ext4/xfs → zswap+swapfc, other → zram | Most users |
| `zswap+swapfc` | Zswap cache + dynamic swap files | **Desktop** |
| `zram+swapfc` | Zram primary + swap files overflow | Memory-constrained |
| `zram` | Zram only (no disk swap) | Unsupported filesystems |
| `manual` | Use explicit settings | Advanced users |

### How Each Mode Works

**zswap + swapfc (default for btrfs/ext4/xfs)**:
- Zswap compresses pages in RAM before writing to swap
- SwapFC creates pre-allocated swap files (512MB each)
- Disk space is only used when zswap pool is full
- Best desktop performance with efficient disk usage

**zram + swapfc**:
- Zram creates compressed block device in RAM (highest priority)
- SwapFC provides overflow to disk when zram is full
- Optional: zram writeback moves idle pages to disk

**zram only**:
- All swap in compressed RAM
- No disk I/O, ideal for Live USB

## Installation

```bash
# Build (requires Rust 1.70+)
cargo build --release

# Install
sudo make install

# Enable
sudo systemctl enable --now systemd-swap
```

### Arch Linux / BigLinux

```bash
cd pkgbuild
makepkg -si
```

## Usage

```bash
# Check status (works without root, extra stats when root)
systemd-swap status

# Show help
systemd-swap --help

# Restart after config changes
sudo systemctl restart systemd-swap

# View logs
journalctl -u systemd-swap -f
```

### Status Output (as root)

```
Zswap:
  enabled: true
  compressor: zstd
  zpool: zsmalloc
  max_pool_percent: 45%

  === Pool Statistics (debugfs) ===
  pool_size: 234567890 (223.7 MiB)
  stored_pages: 58432 (228.2 MiB uncompressed)
  pool_utilization: 48%
  compress_ratio: 98%
  
  === Writeback Statistics ===
  written_back_pages: 1234 (4.8 MiB)
  pool_limit_hit: 0

  === Effective Swap Usage ===
  kernel_reported_used: 300.0 MiB
  in_zswap_pool (RAM): 228.2 MiB
  actual_disk_used: 71.8 MiB
  swap_in_ram: 76%
```

## Configuration

Edit `/etc/systemd/swap.conf`:

```ini
################################################################################
# Swap Mode
################################################################################
swap_mode=auto

################################################################################
# Zswap (used in zswap+swapfc mode)
# Modern defaults for desktop Linux (kernel 6.x+)
################################################################################
zswap_compressor=zstd            # lzo lz4 zstd lzo-rle lz4hc (zstd = best)
zswap_max_pool_percent=45        # Max % of RAM for pool (20-50 typical)
zswap_zpool=zsmalloc             # Memory allocator (default upstream)
zswap_shrinker_enabled=1         # Evict cold pages to disk (default since 6.8)
zswap_accept_threshold=90        # Accept threshold after pool full

################################################################################
# Zram (used in zram modes)
################################################################################
zram_size=50%                    # 50%, 1G, 512M, 100%
zram_alg=zstd                    # Compression algorithm
zram_prio=32767                  # Swap priority

# Zram writeback (requires CONFIG_ZRAM_WRITEBACK)
zram_writeback=0                 # 0=disabled, 1=enabled
zram_writeback_dev=              # Partition or empty for auto loop
zram_writeback_size=1G           # Auto backing file size

################################################################################
# SwapFC - Dynamic swap files
################################################################################
swapfc_chunk_size=512M           # Size: 512M, 1G, 10% (of RAM)
swapfc_max_count=32              # Maximum swap files
swapfc_free_ram_perc=35          # Create when free RAM < this %
swapfc_free_swap_perc=25         # Create more when free swap < this %
swapfc_path=/swapfc/swapfile     # Path for swap files

# Pre-allocated files (default) - more stable, no loop device needed
swapfc_use_sparse=0              # 0=pre-allocate (default), 1=sparse

# Btrfs compression mode (experimental) and need use loop device
swapfc_use_btrfs_compression=0   # Double compression: zswap + btrfs
```

## Custom Swap Location

You can place swap files on a different btrfs partition:

```ini
swapfc_path=/mnt/swap-drive/swapfile
```

## Zram Writeback

Move idle/incompressible pages from zram to disk:

```ini
# Enable with auto loop device
zram_writeback=1
zram_writeback_size=2G

# Or use dedicated partition
zram_writeback=1
zram_writeback_dev=/dev/sda5
```

Requires kernel compiled with `CONFIG_ZRAM_WRITEBACK`.

## File Allocation Mode

By default, swap files are **pre-allocated** using `fallocate`:

- Files reserve 512M on disk immediately
- More stable under memory pressure (no loop device needed)
- Better for most desktop and server workloads

To use sparse files (thin provisioning) instead:

```ini
swapfc_use_sparse=1
```

Sparse mode creates files that only allocate disk space when written, but requires a loop device which can cause issues under extreme memory pressure.

## File Locations

| Path | Description |
|------|-------------|
| `/usr/bin/systemd-swap` | Main binary |
| `/etc/systemd/swap.conf` | User configuration |
| `/usr/share/systemd-swap/swap-default.conf` | Default configuration |
| `/run/systemd-swap/` | Runtime data |
| `/swapfc/` | Swap files (default) |

## Requirements

- Linux kernel 5.0+
- Rust 1.70+ (build only)
- btrfs-progs (for btrfs features)
- util-linux (zramctl, losetup)

## License

GPL-3.0-or-later

