// Centralised default values for all configuration keys.
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Every module reads config keys via `config.get("key").unwrap_or(DEFAULT)`.
// Having the defaults here prevents drift between autoconfig, module code,
// swap-default.conf, and the GUI.

// ── Zram ─────────────────────────────────────────────────────────────────────

pub const ZRAM_SIZE: &str = "125%";
pub const ZRAM_ALG: &str = "zstd";
pub const ZRAM_PRIO: i32 = 32767;
pub const ZRAM_MAX_DEVICES: u8 = 8;
pub const ZRAM_EXPAND_THRESHOLD: u8 = 85;
pub const ZRAM_CONTRACT_THRESHOLD: u8 = 20;
pub const ZRAM_EXPAND_COOLDOWN: u64 = 10;
pub const ZRAM_CONTRACT_STABILITY: u64 = 120;
pub const ZRAM_MIN_FREE_RAM: u8 = 15;
pub const ZRAM_CHECK_INTERVAL: u64 = 5;
pub const ZRAM_EXPAND_MIN_RATIO: f64 = 2.0;

// ── Zswap ────────────────────────────────────────────────────────────────────

pub const ZSWAP_COMPRESSOR: &str = "zstd";
pub const ZSWAP_ZPOOL: &str = "zsmalloc";
pub const ZSWAP_MAX_POOL_PERCENT: u32 = 45;
pub const ZSWAP_SHRINKER_ENABLED: &str = "1";
pub const ZSWAP_ACCEPT_THRESHOLD: &str = "80";

// ── SwapFile ─────────────────────────────────────────────────────────────────

pub const SWAPFILE_PATH: &str = "/swapfile";
pub const SWAPFILE_CHUNK_SIZE: &str = "512M";
pub const SWAPFILE_MAX_COUNT: u32 = 28;
pub const SWAPFILE_MIN_COUNT: u32 = 1;
pub const SWAPFILE_FREE_RAM_PERC: u8 = 20;
pub const SWAPFILE_FREE_SWAP_PERC: u8 = 40;
pub const SWAPFILE_REMOVE_FREE_SWAP_PERC: u8 = 70;
pub const SWAPFILE_FREQUENCY: u32 = 1;
pub const SWAPFILE_SHRINK_THRESHOLD: u8 = 30;
pub const SWAPFILE_SAFE_HEADROOM: u8 = 40;
pub const SWAPFILE_NOCOW: &str = "1";

// Free space left on the filesystem after a swap file is created.
//
// Expressed as a percentage of the filesystem, clamped into an absolute band,
// because neither form works alone. A percentage is meaningless on a small
// filesystem (5% of 8GB does not cover one metadata chunk) and excessive on a
// large one (5% of 4TB is 200GB withheld for nothing).
//
// The floor is what btrfs needs to stay healthy: a metadata chunk is 256MiB to
// 1GiB and the global reserve is up to 512MiB, so a filesystem with less than
// that unallocated can fail writes and turn read-only while still reporting
// free space. ext4 and xfs do not fail this way, but both fragment badly near
// full, and a swap file wants extents.
//
// The cap is a judgement rather than a hard requirement: past a few GiB the
// reserve stops protecting the filesystem and starts being arbitrary.
pub const SWAPFILE_MIN_FREE_PERCENT: u64 = 5;
pub const SWAPFILE_MIN_FREE_FLOOR: u64 = 1024 * 1024 * 1024;
pub const SWAPFILE_MIN_FREE_CAP: u64 = 8 * 1024 * 1024 * 1024;

// Unallocated space btrfs must keep so it can still allocate a metadata chunk.
// This is the number statvfs cannot express: free space inside already
// allocated data chunks is reported as available while metadata starves.
pub const SWAPFILE_BTRFS_MIN_UNALLOCATED: u64 = 1024 * 1024 * 1024;

// ── MGLRU (Multi-Gen LRU) ──────────────────────────────────────────────────

pub const MGLRU_MIN_TTL_MS: u32 = 1000;
