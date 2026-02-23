# systemd-swap

Smart dynamic swap management for Linux, written in Rust.

Automatically detects hardware, selects the best swap strategy, and tunes the
kernel for optimal memory management — no manual configuration required.

## How It Works

### Swap Modes

| Mode | Primary | Secondary | Selection |
|------|---------|-----------|-----------|
| `auto` | Auto-detected | Auto-detected | **Default** — recommended |
| `zram+swapfile` | Zram (RAM) | Swap files (disk) | btrfs / ext4 / xfs with free space |
| `zswap+swapfile` | Zswap (kernel) | Swap files (disk) | Large disk, SSD/NVMe |
| `zram` | Zram (RAM) | None | LiveCD, low disk, tmpfs |
| `manual` | Explicit flags | Explicit flags | Advanced users |
| `disabled` | — | — | Service exits cleanly |

### Auto-Detection Logic

In `auto` mode, the daemon checks:

1. **LiveCD?** (tmpfs/squashfs/overlay root) → `zram` only
2. **Filesystem supports swap files?** (btrfs/ext4/xfs) → if no, `zram` only
3. **Free disk ≥ RAM?** → if no, `zram` only
4. **Otherwise** → `zram+swapfile` (zram primary + disk overflow)

### Zram Pool Architecture

The daemon manages a **dynamic pool of zram devices** that expands and
contracts based on demand:

- **Initial pool**: one device per CPU core (max 8 devices)
- **Expansion**: adds a device when pool utilization exceeds 85%
- **Contraction**: removes idle devices when utilization drops below 20% for 120s
- **Monitoring interval**: 5 seconds

Each zram device uses:
- **Algorithm**: zstd (level 3) — best ratio-to-speed balance
- **Disksize**: 150% of RAM (virtual/uncompressed size)
- **No mem_limit**: prevents write errors that block kernel fallback to disk swap
- **Priority**: 32767 (maximum — kernel uses zram before disk swap)

Physical RAM usage is naturally limited by the kernel's memory watermarks
and the daemon's free-RAM guard (adaptive check before each expansion).

**Compression ratios** (typical):
- Desktop workloads: 3–4x
- Server / text-heavy: 5–10x
- Incompressible data (media, encrypted): ~1x

### Swap Files (Overflow)

In `zram+swapfile` mode, swap files provide emergency overflow:

- **Size**: 512MB each, created on demand
- **Maximum**: 28 files (14GB total capacity)
- **Priority**: -1 (kernel only uses when zram is full)
- **NOCOW**: enabled on btrfs (prevents deadlock under pressure)
- **Created when**: free RAM < 20% or free swap < 40%
- **Removed when**: free swap > 70%

### Zswap Mode

In `zswap+swapfile` mode, the kernel's zswap handles compression:

- Compresses pages before writing to disk swap
- Shrinker moves cold compressed pages to disk automatically
- Pool limited to 45% of RAM
- Requires disk-backed swap files as backing storage

## Recommended Kernel Tuning

The following kernel parameters are **not applied by the daemon** — they are
recommendations for optimal performance with zram/zswap. Configure them
via `/etc/sysctl.d/99-swap.conf` or your distribution's tuning service.

### Memory Management

| Parameter | Value | Purpose |
|-----------|-------|---------|
| `vm.swappiness` | 120 (zram+swapfile) / 180 (zram only) | Prefer swap over file cache (zram is in-memory, so swapping is fast) |
| `vm.min_free_kbytes` | 3% of RAM (max 512MB) | Emergency reserve — gives kswapd headroom before OOM |
| `vm.watermark_scale_factor` | 150 (1.5%) | Gap between min/low/high watermarks for kswapd headroom |
| `vm.vfs_cache_pressure` | 75 | Balance between VFS cache retention and anonymous page reclaim |
| `vm.dirty_ratio` | 10 | Max dirty pages before blocking writes (reduces memory pressure) |
| `vm.dirty_background_ratio` | 3 | Start background writeback early |
| `vm.page-cluster` | 0 (zram) / 2 (zswap) | Pages read per swap-in. 0 = page-at-a-time (optimal for zram) |

### Memory Compaction

| Parameter | Value | Purpose |
|-----------|-------|---------|
| `vm.compaction_proactiveness` | 20 | Background defragmentation level. Lower avoids CPU waste with zram-heavy workloads |
| `vm.watermark_boost_factor` | 15000 | Boosts watermarks after fragmentation events for compaction recovery |
| `vm.extfrag_threshold` | 300 | Eagerness to compact vs reclaim. Lower = more willing to compact |

### Transparent Huge Pages

| Parameter | Value | Purpose |
|-----------|-------|---------|
| THP enabled | `madvise` | Only apps requesting huge pages get them — avoids compaction stalls |
| mTHP 64kB | `madvise` | 64kB folios via madvise — reduces swap I/O overhead |

### MGLRU (Multi-Gen LRU)

| Parameter | Value | Purpose |
|-----------|-------|---------|
| `min_ttl_ms` | 1000 | Pages younger than 1s are never reclaimed — protects working set from thrashing |

## Installation

### Arch Linux / BigLinux / Manjaro

```bash
cd pkgbuild
makepkg -si
```

### Manual Build

Requirements: Rust 1.70+, `util-linux`

```bash
cargo build --release
sudo make install
sudo systemctl enable --now systemd-swap
```

## Usage

### Check Status

```bash
systemd-swap status
```

Shows zram pool stats (compression ratio, utilization, device count),
swap file details, and memory breakdown.

### Show Recommended Config

```bash
sudo systemd-swap autoconfig
```

Displays the auto-detected configuration for the current hardware.

### Restart

```bash
sudo systemctl restart systemd-swap
```

### View Logs

```bash
journalctl -u systemd-swap -f
```

## Configuration

Configuration files (in order of priority):

1. `/usr/share/systemd-swap/swap-default.conf` — defaults (do not edit)
2. `/etc/systemd/swap.conf` — user overrides
3. `/etc/systemd/swap.conf.d/*.conf` — drop-in fragments

All options support `${NCPU}` and `${RAM_SIZE}` variables, plus simple
arithmetic with `$(( expr ))`.

### Common Options

**Change swap mode:**
```ini
swap_mode=zram+swapfile    # or: auto, zram, zswap+swapfile, manual, disabled
```

**Customize zram size:**
```ini
zram_size=200%             # Virtual disksize (% of RAM)
```

**Customize swap file location:**
```ini
swapfile_path=/mnt/data/swapfile
```

**Adjust anti-thrashing protection:**
```ini
mglru_min_ttl_ms=3000      # Higher = more protection, less reclaim
```

**Customize zram pool behavior:**
```ini
zram_expand_threshold=90        # Expand pool above this utilization %
zram_contract_threshold=15      # Contract pool below this utilization %
```

### Full Option Reference

See `/usr/share/systemd-swap/swap-default.conf` for all available options
with descriptions.

## Architecture

```
systemd-swap (Rust daemon)
├── main.rs          — CLI (clap), mode dispatch, kernel tuning, THP/MGLRU
├── lib.rs           — Module declarations, global SHUTDOWN flag
├── config.rs        — Config parser (key=value, ${VAR} expansion, arithmetic)
├── autoconfig.rs    — Hardware detection, recommended config generation
├── zram.rs          — Dynamic zram pool (expansion, contraction, monitoring)
├── swapfile.rs      — Dynamic swap file management (NOCOW, loop-backed)
├── zswap.rs         — Zswap kernel module configuration
├── meminfo.rs       — /proc/meminfo parser, effective swap calculation
├── systemd.rs       — Systemd unit generation, sd-notify
└── helpers.rs       — Shared utilities (parse_size, fs detection, logging)
```

### Data Flow

```
Memory pressure (free RAM < threshold)
  → MGLRU protects working set (pages < 1s old)
  → Kernel swaps cold anonymous pages:
      ├─ zram: compress with zstd level 3 → store in RAM
      │   ├─ Pool utilization > 85% → daemon adds zram device
      │   └─ All disksize consumed → kernel falls back to swapfiles
      └─ zswap: compress in kernel pool → shrinker writes back to disk

SwapFile monitor (1s interval):
  ├─ free_ram < 20%  → create 512MB swap file
  ├─ free_ram < 5%   → emergency: create immediately
  └─ free_swap > 70% → remove unused swap file

ZramPool monitor (5s interval):
  ├─ utilization > 85% → add zram device (up to 8)
  └─ utilization < 20% (120s stable) → remove idle device
```

## Features

- **Zero configuration**: works out of the box for any system
- **Dynamic scaling**: creates/removes swap resources on demand
- **MGLRU integration**: protects working set from premature eviction (kernel 6.1+)
- **mTHP support**: 64kB folios for efficient zram swap I/O
- **Zswap disabled for zram**: prevents double compression per kernel docs
- **NOCOW swap files**: safe on btrfs under memory pressure
- **Adopt on restart**: reuses existing zram devices and swap files without swapoff
- **Graceful shutdown**: restores all kernel parameters on stop

## License

GPL-3.0-or-later
