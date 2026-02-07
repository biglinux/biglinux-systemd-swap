# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

systemd-swap is a dynamic swap management daemon for Linux, written in Rust. It automatically configures optimal swap strategies based on the system's filesystem and requirements, managing zswap, zram, and dynamic swap files (SwapFC).

The project replaces a previous Python implementation with a lightweight (~250 KB) Rust binary that provides:
- Auto-detection of optimal swap strategy based on filesystem type
- Zswap (compressed RAM cache) + SwapFC (dynamic swap files)
- Zram (compressed block device in RAM) with optional writeback to disk
- Multi-filesystem support: btrfs, ext4, xfs

## Building and Testing

```bash
# Build the project (requires Rust 1.70+)
cargo build --release

# Install to system (requires root)
sudo make install

# Enable and start the service
sudo systemctl enable --now systemd-swap

# Check service status
systemctl status systemd-swap

# View detailed swap status (with extra stats as root)
systemd-swap status

# View logs
journalctl -u systemd-swap -f
```

## Build for Arch Linux/BigLinux

```bash
cd pkgbuild
makepkg -si
```

## Architecture

### Swap Modes

The daemon operates in one of several modes (auto-detected or manually configured):

1. **auto** (default): Detects filesystem and chooses optimal strategy
   - btrfs/ext4/xfs → zswap+swapfc
   - other filesystems → zram only

2. **zswap+swapfc**: Zswap compresses pages in RAM, SwapFC provides backing swap files
   - Default for btrfs/ext4/xfs systems
   - Zswap shrinker proactively moves cold pages to disk
   - Best desktop performance with efficient memory usage

3. **zram+swapfc**: Zram as primary compressed swap with swap files for overflow
   - Optional zram writeback moves idle pages to disk

4. **zram**: Zram-only mode (no disk swap)

5. **manual**: Explicit configuration via config file flags

### Core Components

**src/main.rs** (479 lines)
- Entry point with CLI parsing (start/stop/status commands)
- Swap mode selection logic based on filesystem detection
- Main daemon loop orchestrating zswap/zram/swapfc
- Signal handling for graceful shutdown

**src/config.rs** (172 lines)
- Configuration parser supporting hierarchical config files:
  - `/usr/share/systemd-swap/swap-default.conf` (defaults)
  - `/etc/systemd/swap.conf` (user overrides)
  - `{/usr/lib,/run,/etc}/systemd/swap.conf.d/*.conf` (fragments)
- Shell variable expansion in config values
- Type-safe getters for configuration values

**src/swapfc.rs** (558 lines)
- Dynamic swap file management
- Creates/removes swap files based on memory pressure
- Supports both pre-allocated (fallocate) and sparse files
- Btrfs-specific: subvolume setup, compression mode support
- Auto-detects filesystem type and validates support (btrfs/ext4/xfs)

**src/zswap.rs** (265 lines)
- Zswap configuration via `/sys/module/zswap/parameters`
- Parameter backup/restore on daemon stop
- Status reporting with debugfs statistics (pool size, compression ratio, etc.)

**src/zram.rs** (380 lines)
- Zram device creation and configuration
- Writeback support (CONFIG_ZRAM_WRITEBACK kernel feature)
- Auto-creation of loop devices for writeback when no partition specified
- Size parsing: absolute (1G, 512M) or percentage (50%, 100%)

**src/systemd.rs** (145 lines)
- Systemd integration: sd_notify, swap unit generation
- Swap unit management via systemctl

**src/meminfo.rs** (300 lines)
- Memory statistics parsing from `/proc/meminfo`
- CPU count, RAM size, free RAM/swap percentage calculations
- Page size detection

**src/helpers.rs** (148 lines)
- Common utilities: file I/O, directory creation, command execution
- Root privilege checking

**src/pre-systemd-swap** (124 lines, bash)
- Pre-service script run before main daemon
- Btrfs subvolume setup for swap files
- Disables existing swap partitions
- Adds fstab entries for swap subvolumes

### Configuration Flow

1. Load defaults from `/usr/share/systemd-swap/swap-default.conf`
2. Override with `/etc/systemd/swap.conf`
3. Apply fragments from conf.d directories (etc > run > lib precedence)
4. Expand shell variables in values

### Filesystem Detection Logic

- Uses `findmnt` to detect filesystem type
- For swap file paths, checks both the path and its parent
- Validates filesystem against `SWAPFILE_SUPPORTED_FS` (btrfs/ext4/xfs)
- Falls back to zram-only for unsupported filesystems

### Pre-allocated vs Sparse Files

By default, SwapFC uses **pre-allocated** files via fallocate (disabled sparse):
- More stable under memory pressure
- Reserves disk space upfront
- Set `swapfc_use_sparse=1` to enable sparse/thin provisioning

### Btrfs-Specific Handling

- Automatically creates `@swapfc` subvolume on btrfs root
- Optional compression mode: creates loop device over file on compressed subvolume
- Disables CoW (copy-on-write) for swap files

## File Locations

| Path | Description |
|------|-------------|
| `/usr/bin/systemd-swap` | Main Rust binary |
| `/usr/bin/pre-systemd-swap` | Bash pre-setup script |
| `/etc/systemd/swap.conf` | User configuration |
| `/usr/share/systemd-swap/swap-default.conf` | Default configuration |
| `/run/systemd/swap/` | Runtime state directory |
| `/swapfc/` | Default swap file location |
| `/usr/lib/systemd/system/systemd-swap.service` | Main systemd unit |
| `/usr/lib/systemd/system/pre-systemd-swap.service` | Pre-setup systemd unit |

## Key Configuration Parameters

**swap_mode**: auto | zswap+swapfc | zram+swapfc | zram | manual

**Zswap (for zswap+swapfc mode)**:
- `zswap_compressor`: zstd (default), lzo, lz4, lzo-rle, lz4hc
- `zswap_max_pool_percent`: Max RAM % for pool (default: 45)
- `zswap_shrinker_enabled`: Proactive writeback (default: 1)

**Zram (for zram modes)**:
- `zram_size`: 1G, 512M, 50%, 100% (default: 80%)
- `zram_alg`: Compression algorithm (default: zstd)
- `zram_writeback`: Enable writeback to disk (default: 0)

**SwapFC**:
- `swapfc_chunk_size`: Per-file size, e.g., 512M, 10% of RAM
- `swapfc_path`: Location for swap files (default: /swapfc/swapfile)
- `swapfc_free_ram_perc`: Trigger creation when free RAM < % (default: 35)
- `swapfc_use_sparse`: Enable sparse files (default: 0)

## Dependencies

- Rust 1.70+ (build time)
- `libsystemd` (for sd_notify integration)
- `btrfs-progs` (for btrfs features)
- `util-linux` (zramctl, losetup, findmnt)

## Testing Configuration Changes

After modifying `/etc/systemd/swap.conf`:

```bash
sudo systemctl restart systemd-swap
journalctl -u systemd-swap -f
systemd-swap status
```

## Package Building

The project uses GitHub Actions to trigger builds on push:
- Webhook dispatches to BigLinux-Package-Build repositories
- Builds for both x86_64 and aarch64 (ARM)

## Code Style Notes

- Error handling uses `thiserror` crate for typed errors
- Logging via custom macros: `info!()`, `warn!()`, `error!()`, `debug!()`
- Systemd integration via `libsystemd` crate
- Signal handling for graceful shutdown (SIGTERM, SIGINT)
- Configuration uses lazy string parsing with typed getters
