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
