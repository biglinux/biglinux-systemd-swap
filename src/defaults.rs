// Centralised default values for all configuration keys.
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Every module reads config keys via `config.get("key").unwrap_or(DEFAULT)`.
// Having the defaults here prevents drift between autoconfig, module code,
// swap-default.conf, and the GUI.

// ── Zram ─────────────────────────────────────────────────────────────────────

pub const ZRAM_SIZE: &str = "150%";
pub const ZRAM_ALG: &str = "zstd";
pub const ZRAM_PRIO: i32 = 32767;
// Hard ceiling on the physical RAM the zsmalloc pool may hold, as a percentage
// of MemTotal. `disksize` is virtual capacity and bounds nothing: the kernel's
// only RAM ceiling for zram is the `mem_limit` sysfs node, documented as "the
// maximum amount of memory ZRAM can use to store the compressed data". Left at
// 0 the pool grows until reclaim cannot make progress -- measured on a 31 GB
// host, 17.8 GB of zsmalloc pages against 1.1 GB free, kswapd spinning in
// direct reclaim with no OOM kill and no journal line before the reset.
// 50% matches the ArchWiki starting point and scales across RAM sizes.
pub const ZRAM_MEM_LIMIT: &str = "50%";
pub const ZRAM_MAX_DEVICES: u8 = 8;
pub const ZRAM_EXPAND_THRESHOLD: u8 = 85;
pub const ZRAM_CONTRACT_THRESHOLD: u8 = 20;
pub const ZRAM_EXPAND_COOLDOWN: u64 = 10;
pub const ZRAM_CONTRACT_STABILITY: u64 = 120;
pub const ZRAM_CHECK_INTERVAL: u64 = 5;

// ── Zram writeback ───────────────────────────────────────────────────────────
// A page that enters zram never leaves RAM on its own. The pool shrinks only
// when a page is faulted back in or its owner exits, so a working set that
// turned over hours ago keeps its compressed copy resident forever. Measured on
// a 31 GB host two days into a session: of 5,692,421 stored pages, 62.8% had
// not been read for over 24 hours -- 13.6 GB uncompressed, about 4.6 GB of real
// RAM, held by data nothing had touched in a day, while 4.1 GB was free.
//
// The kernel can move those pages to a disk-backed device
// (CONFIG_ZRAM_WRITEBACK) and hand the RAM back, which is what these keys
// drive. The cost is one disk read if such a page is ever wanted again, so the
// pass only ever targets entries the kernel itself reports as idle.
//
// Size is the pool's total backing store as a percentage of RAM, split across
// the devices; 0 disables writeback. It is a ceiling, not an allocation: the
// files are sparse and stay empty until cold pages actually land in them.
//
// 50% is what it takes to reach the cold pages, because the backing store holds
// one uncompressed 4 KiB block per page whatever its compressed size. Measured
// end to end on 7.1: a pass wrote 2121 pages, which freed 2.8 MB of pool RAM and
// allocated 8.3 MB of disk. So disk spent runs about the compression ratio times
// RAM freed, and half of RAM buys back roughly what the >24h-idle set holds.
pub const ZRAM_WRITEBACK_SIZE: &str = "50%";
// The backing files live alongside the swap files, in `swapfile_path`, because
// that directory is already everything they need and there is no second place
// worth maintaining: pre-systemd-swap creates and mounts it before the daemon
// starts, it is a btrfs subvolume with NOCOW, it is named in
// the unit's ReadWritePaths, and it sits outside snapshots of the root.
//
// Sharing it is safe in both directions. swapfc only ever deletes entries whose
// name parses as a number, and these are named `zram-writeback-<id>`; the one
// place it used to delete wholesale now spares an occupied directory. The pool
// starts before swapfc, so on a fresh boot these files can already be present
// when swapfc looks -- which is exactly the case that used to destroy them.
//
// There is no separate constant: the path follows `swapfile_path` so that
// moving the swap files moves these with them.
// Pool RAM usage, as a percentage of `zram_mem_limit`, above which a pass may
// run. Below a fifth of its own budget the pool is not what is making RAM
// scarce, and the pass would only trade free disk and a future page fault for
// memory nothing is asking for.
//
// The percentage used to be of MemTotal, which with a 50% ceiling made the
// shipped 25 mean half the budget. The reference host sat at 12.3% of MemTotal
// for an entire session and the pass never ran once -- `bd_stat` read `0 0 0`
// on every device although each had a 3965 MB store attached at boot -- while
// 62.8% of the stored pages had gone unread for over 24 hours. Against the
// budget the same host reads 24.6% and the pass runs.
pub const ZRAM_WRITEBACK_THRESHOLD: u8 = 20;
// Seconds a page must go untouched to qualify. Anonymous memory idle for an
// hour is cold by any measure, and the kernel tracks the age itself.
pub const ZRAM_WRITEBACK_IDLE: u64 = 3600;
// Seconds between passes. Each one is synchronous, so this also spaces out the
// I/O rather than draining the pool in a single burst.
pub const ZRAM_WRITEBACK_INTERVAL: u64 = 300;
// Pages one device may write per pass (32768 x 4 KiB = 128 MiB). The kernel's
// writeback_limit enforces it, which bounds how long a single pass occupies the
// monitor thread -- on a rotational disk an unbounded pass would otherwise run
// for minutes and delay shutdown.
pub const ZRAM_WRITEBACK_PAGES_PER_PASS: u64 = 32768;

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

// ── MGLRU (Multi-Gen LRU) ──────────────────────────────────────────────────

pub const MGLRU_MIN_TTL_MS: u32 = 1000;
