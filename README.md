# systemd-swap

Smart dynamic swap management for Linux, written in Rust.

## Features

- **Auto-detection**: Automatically chooses optimal swap strategy for your filesystem
- **Zswap + SwapFC**: Compressed RAM cache with dynamic btrfs swap files
- **Zram + SwapFC**: Alternative mode with zram as primary swap
- **Zram writeback**: Move idle pages from zram to disk (kernel 5.4+)
- **Sparse files (thin provisioning)**: Swap files don't pre-allocate disk space
- **Btrfs optimized**: Optional compression mode for swap data
- **Lightweight**: ~250 KB binary vs ~10 MB Python version

## Swap Modes

| Mode | Description | Best For |
|------|-------------|----------|
| `auto` | Auto-detect: btrfs → zswap+swapfc, other → zram | Most users |
| `zswap+swapfc` | Zswap cache + dynamic swap files | **Desktop (btrfs)** |
| `zram+swapfc` | Zram primary + swap files overflow | Memory-constrained |
| `zram` | Zram only (no disk swap) | Non-btrfs systems |
| `manual` | Use explicit settings | Advanced users |

### How Each Mode Works

**zswap + swapfc (default for btrfs)**:
- Zswap compresses pages in RAM before writing to swap
- SwapFC creates swap files as sparse files (thin provisioning)
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
# Check status (run as root for detailed zswap stats)
sudo systemd-swap status

# View available compression algorithms
systemd-swap compression

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
################################################################################
zswap_compressor=zstd            # lzo lz4 zstd lzo-rle lz4hc
zswap_max_pool_percent=45        # Max % of RAM for pool
zswap_zpool=zsmalloc             # Memory allocator
zswap_shrinker_enabled=1         # Move cold pages to disk
zswap_accept_threshold=80        # Accept threshold after pool full

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
swapfc_chunk_size=512M           # Size of each swap file
swapfc_max_count=32              # Maximum swap files
swapfc_free_ram_perc=35          # Create when free RAM < this %
swapfc_free_swap_perc=25         # Create more when free swap < this %
swapfc_path=/swapfc/swapfile     # Path for swap files

# Sparse files (thin provisioning) - ENABLED BY DEFAULT
# Swap files appear full size but only allocate disk space when written.
# Ideal with zswap: pages stay in RAM, disk is only used on writeback.
# swapfc_use_sparse_disable=1    # Uncomment to pre-allocate disk space

# Btrfs compression mode (experimental)
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

## Sparse Files (Thin Provisioning)

By default, swap files are created as sparse files:

- Files appear as 512M but start with 0 bytes on disk
- Disk blocks are allocated only when data is actually written
- With zswap, most pages stay compressed in RAM
- Disk is only used when zswap pool is full (writeback)

To disable and pre-allocate all disk space:

```ini
swapfc_use_sparse_disable=1
```

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

