// Zram configuration for systemd-swap
// Dynamic multi-ZRAM pool with adaptive expansion/contraction
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::config::{Config, WORK_DIR};
use crate::defaults;
use crate::helpers::{
    force_remove, get_fstype, makedirs, parse_size, read_file, run_cmd_output, MB,
};
use crate::systemd::{activate_swap, gen_swap_unit, systemctl, SystemctlAction};
use crate::{debug, error, info, warn};

const ZRAM_MODULE: &str = "/sys/module/zram";
const ZRAM_HOT_ADD: &str = "/sys/class/zram-control/hot_add";
const ZRAM_HOT_REMOVE: &str = "/sys/class/zram-control/hot_remove";

/// Devices the pool starts with.
///
/// One. Compression streams have been per-CPU since Linux 4.7, so a second
/// device buys no compression throughput -- `max_comp_streams` became a no-op
/// in that release and was removed outright in 6.15; the file does not exist on
/// 7.1. What another device does buy is cost: hot_add, a loop, mkswap, a
/// generated unit, and a `daemon-reload` measured at 1.4-1.9 s while the
/// machine is under memory pressure.
///
/// The pool still grows by adding devices, because that is the only way to grow
/// at all: `disksize` is write-once (`-EBUSY` afterwards) and changing it means
/// `reset`, which the kernel documents as freeing everything the device holds.
const INITIAL_DEVICES: u32 = 1;

/// Marks a file in the writeback directory as one this pool created.
const WRITEBACK_PREFIX: &str = "zram-writeback-";

/// RAM the pool can still take if everything that arrives next is
/// incompressible, less 5% for zsmalloc's own overhead.
fn backable_bytes(mem_limit: u64, phys: u64) -> u64 {
    (mem_limit.saturating_sub(phys) as u128 * 95 / 100) as u64
}

/// `disksize` the pool may add now, or 0 when the step is too small to be worth
/// a hot_add, a loop, an mkswap and a `daemon-reload`.
///
/// Every free slot must stay backable at 1:1. A device that reaches its
/// `mem_limit` refuses writes, and the kernel keeps retrying it instead of
/// falling through to disk swap: a pool sized on a 3.35x ratio hung the 4 GiB
/// notebook on the following random-data load, with 855,133 failed writes and
/// untouched disk swap. So a ratio buys nothing in advance. What compression
/// does buy is RAM already saved: a stored page that compressed 4x left three
/// quarters of a page in the budget, and that is room for slots that stay safe
/// whatever arrives next. The pool therefore grows as compressible data lands,
/// and stops growing the moment it stops compressing.
///
/// A `mem_limit` of 0 means the operator asked for no RAM ceiling; only `room`,
/// what is left under `zram_size`, bounds the pool then.
fn expansion_step(mem_limit: u64, phys: u64, free_slots: u64, room: u64, min_step: u64) -> u64 {
    let step = if mem_limit == 0 {
        room
    } else {
        backable_bytes(mem_limit, phys)
            .saturating_sub(free_slots)
            .min(room)
    };
    if step < min_step {
        0
    } else {
        step
    }
}

/// Per-device `mem_limit`s, in whole kernel pages, that make the pool's RAM
/// ceiling bind only when the pool as a whole reaches it.
///
/// The kernel has no pool-wide limit, only one per device, and splitting the
/// budget into shares cannot follow churn: a device's pages leave and arrive
/// faster than any tick, and each stale share is a device refusing writes with
/// the pool under budget. Measured on the notebook, shares recomputed every
/// 5 s logged 80-140 write errors per mixed load with the pool at most at 1710
/// of 1886 MB -- a writeback pass under pressure had held the monitor for 70 s.
/// So each device may take what it holds plus everything the pool has left.
/// Devices have distinct priorities and the kernel writes one at a time, so
/// the limits bind when the pool total does; a stale set can overshoot only by
/// the headroom left when it was written, which is small exactly when it
/// matters. Over budget, which changing `mem_limit` cannot fix because it
/// evicts nothing, every device is held below what it has.
///
/// At least 1 MiB of headroom each: zsmalloc allocates in batches before old
/// pages are freed, and a one-page margin caused five write errors during
/// readback on the notebook. Zero would disable a device's limit, hence
/// `None` when the budget cannot give every device a page.
fn mem_limit_shares(stats: &[ZramStats], budget: u64, page: u64) -> Option<Vec<u64>> {
    let pages = budget / page;
    let count = stats.len() as u64;
    if count == 0 || pages < count {
        return None;
    }
    let held: Vec<u64> = stats
        .iter()
        .map(|s| s.mem_used_total.div_ceil(page).max(1))
        .collect();
    let used: u128 = held.iter().map(|&p| p as u128).sum();
    if used > pages as u128 {
        return Some(
            held.iter()
                .map(|&h| (1 + (pages - count) as u128 * h as u128 / used) as u64 * page)
                .collect(),
        );
    }
    let spare = (pages - used as u64).max(MB.div_ceil(page));
    Some(
        held.iter()
            .map(|&h| (h + spare).min(pages) * page)
            .collect(),
    )
}

/// The block device a zram writes idle pages out to, if it was given one.
///
/// The kernel prints the path as seen from the mount namespace that opened it,
/// and every start of the unit gets a namespace of its own. A device one
/// instance set up reads `/dev/loop3` to it but `/loop3` to the next instance,
/// which adopts that device, so a loop is named by its node, not its path.
/// Matching the path instead leaked one loop, plus the deleted 1.8 GB store it
/// held, on every restart of the test notebook, and would let the orphan sweep
/// take an adopted device's store for an orphan.
fn backing_dev_of(sysfs_path: &str) -> Option<String> {
    let value = std::fs::read_to_string(format!("{}/backing_dev", sysfs_path)).ok()?;
    let value = value.trim();
    if value.is_empty() || value == "none" {
        return None;
    }
    let node = value.rsplit('/').next().unwrap_or(value);
    let is_loop = node
        .strip_prefix("loop")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    Some(if is_loop {
        format!("/dev/{node}")
    } else {
        value.to_string()
    })
}

/// Bytes one `bd_stat` unit stands for. The kernel documents the field in 4 KiB
/// blocks regardless of the architecture's page size, so this is not
/// `get_page_size()`.
const BD_BLOCK_BYTES: u64 = 4096;

/// Pages currently held on a device's backing store (`bd_stat` field 0, 4 KiB units).
fn bd_count(sysfs_path: &str) -> u64 {
    std::fs::read_to_string(format!("{}/bd_stat", sysfs_path))
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

/// Bytes of writeback store one device may have, from the pool total, the
/// device's own capacity, and the free space on the filesystem holding it.
///
/// Three guards, all of which have to hold on hardware this package cannot see:
/// never claim more than a quarter of what is actually free, because plenty of
/// these hosts run a root filesystem that is already near full; never claim more
/// than the device can ever put there, since every entry in the store came out
/// of that one device; and give up rather than hand out a share too small to be
/// worth a loop device and a file.
///
/// The `disksize` cap is what replaced a fixed division by the initial device
/// count: that arithmetic did not know how many devices existed, so a fifth
/// device was handed a fifth quarter of the total. Re-reading free space per
/// device bounds the aggregate instead, and the files are sparse, so an
/// unclaimed share costs nothing.
fn writeback_share(total: u64, free_bytes: u64, disksize: u64) -> u64 {
    const MIN_USEFUL: u64 = 256 * MB;

    if total == 0 {
        return 0;
    }
    let share = total.min(free_bytes / 4).min(disksize);
    if share < MIN_USEFUL {
        0
    } else {
        share
    }
}

/// Release the loop device a zram was using for writeback.
///
/// Has to run before the device is reset: the reset clears `backing_dev`, and
/// after that nothing records which loop to free, so it would stay attached to a
/// file no one deletes.
fn detach_backing_dev(sysfs_path: &str) {
    let Some(dev) = backing_dev_of(sysfs_path) else {
        return;
    };
    // Only ever release a loop we could have created. An administrator who
    // pointed `zram_writeback_path` at a real partition keeps it.
    if !dev.starts_with("/dev/loop") {
        return;
    }
    if let Err(e) = run_cmd_output(&["losetup", "-d", &dev]) {
        warn!("Zram: detaching {} from {} failed: {}", dev, sysfs_path, e);
    }
}

#[derive(Error, Debug)]
pub enum ZramError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Helper error: {0}")]
    Helper(#[from] crate::helpers::HelperError),
    #[error("Systemd error: {0}")]
    Systemd(#[from] crate::systemd::SystemdError),
    #[error("Zram module not available")]
    NotAvailable,
    #[error("zramctl failed: {0}")]
    ZramctlFailed(String),
    #[error("Pool max devices reached")]
    PoolMaxDevices,
}

pub type Result<T> = std::result::Result<T, ZramError>;

/// Check if zram is available
pub fn is_available() -> bool {
    Path::new(ZRAM_MODULE).is_dir()
}

/// Set comp_algorithm for a ZRAM device.
fn configure_zram_algorithm(sysfs: &str, comp_alg: &str, ctx: &str) {
    let comp_path = format!("{}/comp_algorithm", sysfs);
    if let Err(e) = std::fs::write(&comp_path, comp_alg) {
        warn!("{}: failed to set comp_algorithm: {}", ctx, e);
    }
}

/// Deinitialise a zram device that was in use as swap.
///
/// Closing a device that was open for writing makes udev probe it (its
/// `watch` rule), and the kernel refuses `reset` with EBUSY while anything
/// holds the device open. Straight after `swapoff` that probe is usually
/// running: the reset failed that way on the host at shutdown and on the test
/// notebook at a restart, the `change` event landing ~15 ms after the swapoff,
/// and the device stayed initialised for the next start to trip over. The
/// probe takes milliseconds, so wait it out briefly instead of giving up.
fn reset_after_swapoff(sysfs: &str) -> std::io::Result<()> {
    let path = format!("{}/reset", sysfs);
    for _ in 0..20 {
        match std::fs::write(&path, "1") {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                thread::sleep(Duration::from_millis(50))
            }
            result => return result,
        }
    }
    std::fs::write(&path, "1")
}

// =============================================================================
// ZramPool — Dynamic Multi-ZRAM Device Manager
// =============================================================================

/// State of a single ZRAM device in the pool
#[derive(Debug, Clone, Copy, PartialEq)]
enum ZramDeviceState {
    Active,
    Draining, // swapoff in progress
}

/// A single ZRAM device managed by the pool
#[derive(Debug)]
struct ZramDevice {
    /// Kernel device ID (zram0 → 0, zram1 → 1)
    id: u32,
    /// Configured disksize in bytes
    disksize: u64,
    /// sysfs path (e.g., /sys/block/zram0)
    sysfs_path: String,
    /// Device path (e.g., /dev/zram0)
    dev_path: String,
    /// Systemd swap unit name
    unit_name: String,
    /// Device state
    state: ZramDeviceState,
    /// Swapoff attempt count while in Draining state
    drain_attempts: u32,
}

/// Aggregated statistics from all active ZRAM devices in the pool
#[derive(Debug, Clone)]
pub struct ZramPoolStats {
    pub device_count: u8,
    pub total_disksize: u64,
    pub total_orig_data: u64,
    pub total_phys_used: u64,
    pub compression_ratio: f64,
    /// Resident original bytes divided by allocated RAM, including allocator
    /// overhead. Both ratios exclude writeback; compression_ratio uses only
    /// compressed payload bytes as its denominator.
    pub sizing_ratio: f64,
    pub utilization_percent: u8,
    pub phys_usage_percent: u8,
}

/// Configuration for the ZramPool
#[derive(Debug, Clone)]
pub struct ZramPoolConfig {
    /// Maximum number of ZRAM devices (1-8)
    pub max_devices: u8,
    /// `zram_size`: the largest total `disksize` the pool may ever reach, as a
    /// percentage of RAM. With a `zram_mem_limit` the pool is further capped at
    /// 95% of that limit, so slots stay backable even at 1:1 compression; with
    /// none, this is the pool's size.
    pub size_ceiling_percent: u32,
    /// Compression algorithm
    pub algorithm: String,
    /// Swap priority of the first device; each later one takes one less
    pub priority: i32,
    /// Per-device mem_limit as percentage of RAM (0 = unlimited)
    /// `zram_mem_limit` resolved to bytes. 0 means the operator asked for no
    /// ceiling. Bytes and not a percentage because `parse_size` accepts both
    /// `50%` and `8G`, and the percent-only parser silently turned every
    /// absolute value into 0 — which, now that the pool derives its `disksize`
    /// from this number, does not merely skip a limit but falls all the way
    /// back to advertising `zram_size` with nothing backing it.
    pub mem_limit_bytes: u64,
    /// Pool utilization % that triggers expansion
    pub expand_threshold: u8,
    /// Pool utilization % below which to contract
    pub contract_threshold: u8,
    /// Seconds between expansion attempts
    pub expand_cooldown: u64,
    /// Seconds utilization must stay low before contraction
    pub contract_stability: u64,
    /// Seconds between monitor checks
    pub check_interval: u64,
    /// Total backing store for writeback, in bytes (0 = writeback disabled)
    pub writeback_total: u64,
    /// Directory holding the per-device backing files
    pub writeback_path: PathBuf,
    /// Pool RAM usage % above which a writeback pass may run
    pub writeback_threshold: u8,
    /// Seconds a page must go untouched to qualify for writeback
    pub writeback_idle: u64,
    /// Seconds between writeback passes
    pub writeback_interval: u64,
}

impl ZramPoolConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_devices: config
                .get_as::<u8>("zram_max_devices")
                .unwrap_or(defaults::ZRAM_MAX_DEVICES)
                .clamp(1, 8),
            // A percentage of RAM, or a size converted to one. Anything else
            // falls back to the shipped 150%: a silent 50% here left pools in
            // explicitly configured modes no room to grow past their first
            // device, while auto mode injected 150%.
            size_ceiling_percent: {
                let raw = config.get_opt("zram_size").unwrap_or(defaults::ZRAM_SIZE);
                raw.strip_suffix('%')
                    .and_then(|p| p.parse().ok())
                    .or_else(|| {
                        let ram = crate::meminfo::get_ram_size().ok()?.max(1);
                        Some((parse_size(raw).ok()? as u128 * 100 / ram as u128) as u32)
                    })
                    .unwrap_or(150)
            },
            algorithm: config
                .get("zram_alg")
                .unwrap_or(defaults::ZRAM_ALG)
                .to_string(),
            priority: config.get_as("zram_prio").unwrap_or(defaults::ZRAM_PRIO),
            expand_threshold: config
                .get_as::<u8>("zram_expand_threshold")
                .unwrap_or(defaults::ZRAM_EXPAND_THRESHOLD)
                .clamp(50, 95),
            contract_threshold: config
                .get_as::<u8>("zram_contract_threshold")
                .unwrap_or(defaults::ZRAM_CONTRACT_THRESHOLD)
                .clamp(5, 50),
            expand_cooldown: config
                .get_as::<u64>("zram_expand_cooldown")
                .unwrap_or(defaults::ZRAM_EXPAND_COOLDOWN)
                .clamp(5, 120),
            contract_stability: config
                .get_as::<u64>("zram_contract_stability")
                .unwrap_or(defaults::ZRAM_CONTRACT_STABILITY)
                .clamp(30, 600),
            check_interval: config
                .get_as::<u64>("zram_check_interval")
                .unwrap_or(defaults::ZRAM_CHECK_INTERVAL)
                .clamp(3, 300),
            mem_limit_bytes: parse_size(
                config
                    .get_opt("zram_mem_limit")
                    .unwrap_or(defaults::ZRAM_MEM_LIMIT),
            )
            .unwrap_or(0),
            writeback_total: parse_size(
                config
                    .get_opt("zram_writeback_size")
                    .unwrap_or(defaults::ZRAM_WRITEBACK_SIZE),
            )
            .unwrap_or(0),
            // Follows swapfile_path so the backing files sit with the swap files
            // they belong beside: one directory that pre-systemd-swap has already
            // created and mounted, that carries NOCOW, and
            // that the hardened unit names in ReadWritePaths. An override has to
            // stay inside that ReadWritePaths list or ProtectSystem=strict will
            // refuse the write and writeback will silently never run.
            //
            // Held to the same boundary swapfc applies to its own directory. The
            // path reaches `btrfs subvolume create`, `chattr` and a sweep that
            // deletes matching files, so a typo naming `/usr` or `/etc` would be
            // acted on. root owns swap.conf, so this is a footgun rather than a
            // privilege boundary -- but swapfc already refuses those paths, and
            // the two keys feeding the same directory must not disagree.
            writeback_path: {
                let raw = config
                    .get_opt("zram_writeback_path")
                    .or_else(|| config.get_opt("swapfile_path"))
                    .unwrap_or(defaults::SWAPFILE_PATH);
                let path = PathBuf::from(raw.trim_end_matches('/'));
                if crate::swapfile::validate_swapfile_path(&path) {
                    path
                } else {
                    warn!(
                        "ZramPool: refusing writeback path {:?}; writeback disabled",
                        path
                    );
                    PathBuf::new()
                }
            },
            writeback_threshold: config
                .get_as::<u8>("zram_writeback_threshold")
                .unwrap_or(defaults::ZRAM_WRITEBACK_THRESHOLD)
                .clamp(5, 90),
            writeback_idle: config
                .get_as::<u64>("zram_writeback_idle")
                .unwrap_or(defaults::ZRAM_WRITEBACK_IDLE)
                .clamp(60, 86400),
            writeback_interval: config
                .get_as::<u64>("zram_writeback_interval")
                .unwrap_or(defaults::ZRAM_WRITEBACK_INTERVAL)
                .clamp(30, 3600),
        }
    }
}

/// Dynamic multi-ZRAM pool manager
pub struct ZramPool {
    devices: Vec<ZramDevice>,
    config: ZramPoolConfig,
    ram_total: u64,
    last_expansion: Option<Instant>,
    last_contraction: Option<Instant>,
    low_util_since: Option<Instant>,
    last_writeback: Option<Instant>,
    /// Cleared for good once the kernel refuses an age-based `idle` mark, which
    /// is the only signal that it was built without CONFIG_ZRAM_TRACK_ENTRY_ACTIME.
    writeback_usable: bool,
}

impl ZramPool {
    /// Create a new ZramPool from configuration
    pub fn new(config: &Config) -> Result<Self> {
        if !is_available() {
            return Err(ZramError::NotAvailable);
        }

        let ram_total = crate::meminfo::get_ram_size()
            .map_err(|e| ZramError::ZramctlFailed(format!("Failed to get RAM size: {}", e)))?;

        let mut pool_config = ZramPoolConfig::from_config(config);

        // A ceiling below the RAM ceiling would make expansion impossible.
        if pool_config.size_ceiling_percent < 50 {
            pool_config.size_ceiling_percent = 50;
        }

        makedirs(format!("{}/zram", WORK_DIR))?;

        Ok(Self {
            devices: Vec::new(),
            config: pool_config,
            ram_total,
            last_expansion: None,
            last_contraction: None,
            low_util_since: None,
            last_writeback: None,
            writeback_usable: true,
        })
    }

    /// Bring up the pool's first device, or adopt what a previous instance left.
    ///
    /// The size it starts at is the whole argument. `disksize` is the swap
    /// capacity the kernel is told about; `mem_limit` is the RAM the pool may
    /// actually consume. Their quotient is a bet on the compression ratio, and
    /// the kernel has no way to learn the bet was wrong: when a write fails
    /// because `mem_limit` is reached, `should_reclaim_retry()` still sees the
    /// unbacked slots as free swap and keeps aiming reclaim at the same device
    /// instead of falling through to the disk tier below it.
    ///
    /// Measured in a VM at 150%/50% -- a bet of 3.0 -- against data compressing
    /// 1.02x: 1991 MB absorbed, 3871 MB of slots advertised that could never be
    /// backed, the disk tier untouched at 0 bytes, 20-30 `Write-error on
    /// swap-device` lines, and the OOM killer firing while the kernel's own
    /// report read `Free swap = 3390220kB`. Halving `mem_limit` alone, on data
    /// compressing 3.04x, reproduced it worse: 1.5 M suppressed write errors and
    /// a hang with no OOM kill at all. The failure tracks the bet, not the data.
    ///
    /// So the first device bets nothing: `disksize` equals the RAM ceiling, less
    /// 5% for zsmalloc bookkeeping, and slots therefore run out no later than
    /// RAM does. Later devices keep that property; see `expansion_step`.
    pub fn start_primary(&mut self) -> Result<()> {
        crate::systemd::notify_status("Setting up ZramPool...");

        let total_disksize = self.initial_disksize();
        if total_disksize == 0 {
            warn!("ZramPool: calculated disksize is 0, skipping");
            return Ok(());
        }

        // Try to adopt existing active zram swap devices first
        let adopted = self.adopt_existing_devices();
        if adopted > 0 {
            info!(
                "ZramPool: adopted {} existing device(s) from a previous instance",
                adopted
            );
        }

        // Only safe once adoption has run: the sweep spares any file whose loop
        // an adopted device is still reading from.
        self.sweep_orphan_backing();

        if self.devices.is_empty() {
            info!(
                "ZramPool: creating the first device ({}MB, alg={}, max_devices={})",
                total_disksize / (1024 * 1024),
                self.config.algorithm,
                self.config.max_devices
            );
            self.create_device(total_disksize)?;
        }

        // Adopted devices came up under whatever ceiling the previous instance
        // used, and `mem_limit` is writable at runtime, so re-share it over the
        // pool as it now stands. Without this an upgrade that adopts four
        // running devices leaves them all unlimited -- the exact machine that
        // most needs the ceiling.
        self.share_mem_limit_across();

        self.save_device_info()?;
        crate::systemd::notify_status("ZramPool: initial devices ready");
        Ok(())
    }

    /// Adopt existing active zram swap devices from a previous instance.
    /// Returns the number of devices adopted.
    fn adopt_existing_devices(&mut self) -> usize {
        let mut adopted: Vec<(i32, ZramDevice)> = Vec::new();
        // Scan /sys/block/zram* for active devices
        let Ok(entries) = std::fs::read_dir("/sys/block") else {
            return 0;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("zram") {
                continue;
            }
            let Some(id_str) = name_str.strip_prefix("zram") else {
                continue;
            };
            let Ok(id) = id_str.parse::<u32>() else {
                continue;
            };

            let sysfs_path = format!("/sys/block/zram{}", id);
            let dev_path = format!("/dev/zram{}", id);

            // Check if this device is currently used as swap
            let disksize_path = format!("{}/disksize", sysfs_path);
            let Ok(disksize_str) = std::fs::read_to_string(&disksize_path) else {
                continue;
            };
            let Ok(disksize) = disksize_str.trim().parse::<u64>() else {
                continue;
            };
            if disksize == 0 {
                continue; // Not initialized
            }

            // Check if it's an active swap device via /proc/swaps.
            // Use exact field match — substring matching would confuse
            // /dev/zram1 with /dev/zram10.
            let Ok(swaps) = std::fs::read_to_string("/proc/swaps") else {
                continue;
            };
            // A device a previous instance activated reads `/zram1`: the
            // kernel prints the path relative to the /dev mount of the
            // namespace that ran swapon, and that namespace ended with it.
            let relative_path = format!("/zram{}", id);
            // Priority is the fifth field. It orders the pool: the RAM split
            // and contraction both assume `devices` runs from the device the
            // kernel writes first to the one it writes last.
            let Some(priority) = swaps.lines().skip(1).find_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let name = fields.first().copied();
                (name == Some(dev_path.as_str()) || name == Some(relative_path.as_str()))
                    .then(|| fields.get(4)?.parse::<i32>().ok())
                    .flatten()
            }) else {
                continue;
            };

            // Find its systemd swap unit if one exists
            let expected_unit = dev_path.trim_start_matches('/').replace('/', "-") + ".swap";
            let unit_name = expected_unit;

            let device = ZramDevice {
                id,
                disksize,
                sysfs_path: sysfs_path.clone(),
                dev_path: dev_path.clone(),
                unit_name,
                state: ZramDeviceState::Active,
                drain_attempts: 0,
            };
            info!(
                "ZramPool: adopted existing zram{} (disksize={}MB)",
                id,
                disksize / (1024 * 1024)
            );
            adopted.push((priority, device));
        }
        adopted.sort_by_key(|(priority, _)| std::cmp::Reverse(*priority));
        let count = adopted.len();
        self.devices
            .extend(adopted.into_iter().map(|(_, device)| device));
        count
    }

    /// RAM the whole pool may hold, in bytes. 0 means the operator asked for no
    /// ceiling, and every decision keyed on it falls back to `zram_size`.
    fn mem_limit_total(&self) -> u64 {
        self.config.mem_limit_bytes
    }

    /// `disksize` for the pool's first device: the RAM ceiling, less 5%.
    ///
    /// The margin covers zsmalloc's own bookkeeping, which is why the pool's
    /// physical use is never quite its stored size -- measured at phys/orig 0.98
    /// on incompressible data. Erring low keeps slots the binding limit, so the
    /// kernel runs out of somewhere to put the page before it runs out of RAM to
    /// put it in, and falls through to the disk tier on its own.
    ///
    /// With no ceiling configured there is nothing to derive from and the
    /// operator's `zram_size` is used as-is.
    fn initial_disksize(&self) -> u64 {
        expansion_step(self.mem_limit_total(), 0, 0, self.size_ceiling(), 0)
    }

    /// The part of the RAM ceiling expansion may commit: 65%. The rest is a
    /// reserve against churn, which nothing can undo while it happens: when
    /// compressible pages fault back in, their slots come free with a fraction
    /// of a page of RAM behind each, and the kernel refills them with whatever
    /// it is swapping out. `relieve_overcommit` writes the incompressible ones
    /// out, but under the pressure that causes churn writeback ran at ~3 MB/s
    /// on the notebook. Re-reading a 3 GB compressible + 2.5 GB random working
    /// set there churned ~530 MB within a minute: a 20% reserve (377 MB)
    /// logged 50 write errors. 35% covers it, and costs little -- at 4x the
    /// pool still grows to about 2.5 times the ceiling, 125% of RAM with the
    /// defaults, where a pool that never grows stops at 47.5%.
    ///
    /// The first device needs no reserve -- a lone device's free slots can
    /// never outgrow the RAM left under its ceiling -- so it is sized from the
    /// whole ceiling. 0 still means no ceiling.
    fn growth_budget(&self) -> u64 {
        (self.mem_limit_total() as u128 * 65 / 100) as u64
    }

    /// `zram_size` in bytes: the most `disksize` the whole pool may reach.
    fn size_ceiling(&self) -> u64 {
        self.ram_total * self.config.size_ceiling_percent as u64 / 100
    }

    /// Write `mem_limit_shares` to every device. Draining devices keep theirs
    /// until swapoff actually succeeds.
    fn share_mem_limit_across(&self) {
        let total = self.mem_limit_total();
        if total == 0 || crate::is_dry_run() {
            return;
        }
        let Some(stats): Option<Vec<_>> = self
            .devices
            .iter()
            .map(|d| get_device_stats(&d.sysfs_path, d.disksize))
            .collect()
        else {
            warn!("ZramPool: cannot redistribute RAM without every device's statistics");
            return;
        };
        let Some(shares) = mem_limit_shares(&stats, total, crate::meminfo::get_page_size()) else {
            warn!("ZramPool: RAM budget cannot provide one page per device");
            return;
        };
        let mut updates: Vec<_> = self.devices.iter().zip(stats).zip(shares).collect();
        // Complete decreases before increases. A failed decrease must not be
        // followed by spending the RAM it was supposed to release.
        updates.sort_by_key(|((_, st), share)| st.mem_limit != 0 && *share > st.mem_limit);
        for ((dev, st), share) in updates {
            if share == st.mem_limit {
                continue;
            }
            if let Err(e) =
                std::fs::write(format!("{}/mem_limit", dev.sysfs_path), share.to_string())
            {
                warn!(
                    "ZramPool: failed to set mem_limit on {}: {}",
                    dev.sysfs_path, e
                );
                return;
            }
        }

        debug!(
            "ZramPool: mem_limit = {}MB pool-wide across {} device(s)",
            total / (1024 * 1024),
            self.devices.len()
        );
    }

    // ── Writeback backing store ──────────────────────────────────────────────

    /// Path of the backing file belonging to one device.
    fn backing_file(&self, id: u32) -> PathBuf {
        self.config
            .writeback_path
            .join(format!("{}{}", WRITEBACK_PREFIX, id))
    }

    /// Free a device's writeback store: detach its loop, delete its file.
    ///
    /// Call before resetting the device, while `backing_dev` still names the loop.
    fn release_backing_store(&self, sysfs_path: &str, id: u32) {
        detach_backing_dev(sysfs_path);
        force_remove(self.backing_file(id), false);
    }

    /// Prepare the directory the backing files live in.
    ///
    /// On btrfs it is made a subvolume so the several GB of cold pages it can
    /// hold stay out of every snapshot taken of the root, and it carries `+C`
    /// so the loop writes skip copy-on-write. Allocating a fresh extent per
    /// write is what deadlocks btrfs when the writes are themselves part of
    /// freeing memory, which is exactly when writeback runs.
    fn prepare_writeback_dir(&self) -> bool {
        let path = &self.config.writeback_path;
        let is_btrfs = get_fstype(path).as_deref() == Some("btrfs");

        if !path.is_dir() {
            // `btrfs subvolume create` will not create intermediate components,
            // and on a fresh install the package's state directory is the
            // component that does not exist yet.
            if let Some(parent) = path.parent() {
                let _ = makedirs(parent);
            }
            let made_subvolume = is_btrfs
                && Command::new("btrfs")
                    .args(["subvolume", "create"])
                    .arg(path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);

            if !made_subvolume {
                if is_btrfs {
                    warn!(
                        "ZramPool: {} is not a subvolume; root snapshots will include \
                         written-back pages",
                        path.display()
                    );
                }
                if let Err(e) = makedirs(path) {
                    warn!("ZramPool: cannot create {}: {}", path.display(), e);
                    return false;
                }
            }
        }

        // Inherited by files created afterwards, which is every backing file we
        // make, so it is worth setting even on a directory that already existed.
        if is_btrfs {
            let _ = Command::new("chattr")
                .args(["+C"])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        true
    }

    /// Bytes of backing store one device should get, or 0 to go without.
    ///
    /// Bounded by the configured pool total, by the device's own `disksize`
    /// (nothing else can ever reach this store), and by a quarter of what the
    /// filesystem actually has free: the files are sparse, but this package runs
    /// on plenty of hosts whose root is already near full, and there a smaller
    /// writeback window is a far better outcome than filling the disk.
    fn writeback_size_per_device(&self, disksize: u64) -> u64 {
        // Empty means the configured path failed validation; fail closed.
        if self.config.writeback_path.as_os_str().is_empty() {
            return 0;
        }
        let Ok(stat) = nix::sys::statvfs::statvfs(&self.config.writeback_path) else {
            return 0;
        };
        // Saturating: a filesystem that reports nonsense for either field (some
        // network and FUSE mounts do) must not wrap this into a small number
        // that then looks like plenty of room.
        let free = stat.blocks_available().saturating_mul(stat.block_size());

        // `writeback_total` is a per-device ceiling here, not a running
        // allowance: subtracting what the other stores hold cannot bound the
        // pool from this point, because the store is sparse and truncated to
        // its full size the instant it is created, so the first device always
        // reads as having claimed everything before it has written a page --
        // that is exactly what starved the second store to a sliver. The pool
        // total is instead enforced live in `run_writeback`, against the pages
        // each store actually holds (`bd_stat`), which is the only bound that
        // stays true as pages fault back and the count falls again.
        writeback_share(self.config.writeback_total, free, disksize)
    }

    /// Give a device its own disk-backed store. Must run before `disksize`.
    ///
    /// The kernel accepts `backing_dev` only while the device is uninitialised,
    /// only as a block device, and it opens that device exclusively -- measured
    /// on 7.1: a second zram pointed at the same loop is refused with EBUSY, and
    /// a plain file path with ENOTBLK. So one sparse file behind one loop per
    /// device is the only shape available.
    fn attach_backing_dev(&self, sysfs_path: &str, id: u32, disksize: u64) {
        if !Path::new(&format!("{}/backing_dev", sysfs_path)).exists() {
            return; // kernel built without CONFIG_ZRAM_WRITEBACK
        }
        let size = self.writeback_size_per_device(disksize);
        if size == 0 {
            return;
        }
        if crate::is_dry_run() {
            info!(
                "[dry-run] would: back zram{} with a {}MB writeback store",
                id,
                size / MB
            );
            return;
        }
        if !self.prepare_writeback_dir() {
            return;
        }

        let file = self.backing_file(id);
        force_remove(&file, false);

        // Sparse and 0600: the file costs nothing until cold pages land in it,
        // which is what makes a 50%-of-RAM ceiling reasonable to ask for.
        let handle = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&file)
        };
        let sized = handle.and_then(|f| f.set_len(size));
        if let Err(e) = sized {
            warn!("ZramPool: cannot allocate {}: {}", file.display(), e);
            force_remove(&file, false);
            return;
        }

        // direct-io keeps the page cache out of the path. Caching these writes
        // would spend RAM in order to reclaim RAM.
        let loop_dev = match run_cmd_output(&[
            "losetup",
            "-f",
            "--show",
            "--direct-io=on",
            &file.to_string_lossy(),
        ]) {
            Ok(dev) => dev.trim().to_string(),
            Err(e) => {
                warn!("ZramPool: losetup failed for {}: {}", file.display(), e);
                force_remove(&file, false);
                return;
            }
        };

        if let Err(e) = std::fs::write(format!("{}/backing_dev", sysfs_path), &loop_dev) {
            warn!(
                "ZramPool: zram{} refused backing_dev {}: {}",
                id, loop_dev, e
            );
            let _ = run_cmd_output(&["losetup", "-d", &loop_dev]);
            force_remove(&file, false);
            return;
        }

        info!(
            "ZramPool: zram{} writeback store = {}MB on {}",
            id,
            size / MB,
            loop_dev
        );
    }

    /// Drop backing files left behind by a run that did not shut down cleanly.
    ///
    /// A file may only be removed when no zram is currently using the loop that
    /// holds it: devices adopted from a previous instance keep theirs, and
    /// deleting one of those would destroy pages the kernel still believes it
    /// can read back.
    fn sweep_orphan_backing(&self) {
        // Deletes files and detaches loop devices, so it owes the same
        // short-circuit every other mutating path in this crate honours.
        if crate::is_dry_run() {
            info!("[dry-run] would: sweep orphaned writeback stores");
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.config.writeback_path) else {
            return;
        };

        let in_use: Vec<String> = std::fs::read_dir("/sys/block")
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("zram"))
            .filter_map(|e| backing_dev_of(&e.path().to_string_lossy()))
            .collect();

        for entry in entries.flatten() {
            let path = entry.path();
            let is_ours = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(WRITEBACK_PREFIX));
            if !is_ours {
                continue;
            }

            // Ask losetup which loops hold this exact file rather than reading
            // the backing path it prints: on btrfs it reports that path relative
            // to the subvolume root, so comparing strings would match nothing.
            let attached = run_cmd_output(&[
                "losetup",
                "-j",
                &path.to_string_lossy(),
                "-O",
                "NAME",
                "--noheadings",
            ])
            .unwrap_or_default();
            let attached: Vec<&str> = attached.split_whitespace().collect();

            if attached.iter().any(|l| in_use.iter().any(|u| u == l)) {
                continue;
            }
            for loop_dev in attached {
                let _ = run_cmd_output(&["losetup", "-d", loop_dev]);
            }
            info!(
                "ZramPool: removing orphaned writeback store {}",
                path.display()
            );
            force_remove(&path, false);
        }
    }

    /// Move pages the kernel reports as idle out of RAM and onto disk.
    ///
    /// This is the only thing that ever shrinks a live pool. Without it a page
    /// that entered zram stays resident until it is faulted back in or its owner
    /// exits, so a working set that turned over hours ago keeps its compressed
    /// copy in RAM for the rest of the session.
    fn run_writeback(&mut self, stats: &ZramPoolStats) {
        if !self.writeback_usable || self.config.writeback_total == 0 {
            return;
        }
        // Measured against the pool's own ceiling, not against MemTotal.
        // Against MemTotal the shipped 25% meant half the RAM budget, and the
        // reference host sat at 12.3% for the whole session: `bd_stat` read
        // `0 0 0` on every device although each had a 3965 MB store attached at
        // boot, while 62.8% of the stored pages had not been read in 24 hours.
        // The pass is the only thing that lowers `phys` without a `swapoff`, so
        // a gate that never opens is the difference between a pool that gives
        // RAM back and one that does not.
        let budget = self.mem_limit_total();
        let used_percent = if budget > 0 {
            // Clamped, not truncated: with a failed `mem_limit` write a device
            // runs unlimited, and 3x the budget would otherwise come back as
            // 44% and skip the pass exactly when it is most needed.
            (stats.total_phys_used * 100 / budget).min(100) as u8
        } else {
            stats.phys_usage_percent
        };
        if used_percent < self.config.writeback_threshold {
            return;
        }
        if let Some(last) = self.last_writeback {
            if last.elapsed().as_secs() < self.config.writeback_interval {
                return;
            }
        }
        if crate::is_dry_run() {
            return;
        }
        self.last_writeback = Some(Instant::now());

        let targets = self.writeback_targets();
        for (_, sysfs) in &targets {
            // An age in seconds is the whole point of the pass. `all` would mark
            // the live working set idle as well and write it straight to disk. A
            // kernel without CONFIG_ZRAM_TRACK_ENTRY_ACTIME rejects the number,
            // and since there is no safe fallback, writeback stops for good.
            if let Err(e) = std::fs::write(
                format!("{}/idle", sysfs),
                self.config.writeback_idle.to_string(),
            ) {
                warn!(
                    "ZramPool: kernel does not track entry age ({}); writeback disabled",
                    e
                );
                self.writeback_usable = false;
                return;
            }
        }
        self.write_back(&targets, "idle", u64::MAX, stats.total_phys_used);
    }

    /// Move incompressible pages to disk when the pool advertises more free
    /// slots than the RAM left under `growth_budget` can back at 1:1.
    ///
    /// Churn reaches that state faster than the monitor can: compressible pages
    /// faulting back in free slots with a fraction of a page of RAM behind each,
    /// and the kernel refills them with whatever it is swapping out now. On the
    /// notebook a mixed load ate a 20% reserve in a minute -- zram1 lost
    /// 365 MB of 4x data and took 400 MB of random data in its place -- and
    /// logged 140 write errors. Incompressible pages are the ones the kernel
    /// stores whole (`huge`), so each one written back returns a full page of
    /// RAM while its slot stays taken, which is exactly the trade needed. It
    /// needs no idle marking, so it runs on any writeback-capable kernel and
    /// ignores the pass interval.
    ///
    /// Keyed on the growth budget, not the full ceiling: measured against the
    /// ceiling the first pass ran with the pool at 1867 of 1886 MB, one tick
    /// too late, and 40 writes had already failed. Acting as soon as the
    /// reserve is touched leaves the reserve for the churn of one tick.
    fn relieve_overcommit(&mut self, stats: &ZramPoolStats) {
        // A lone device is safe by construction -- its slots never outgrow the
        // RAM under the full ceiling -- and measured against the growth budget
        // it would look overcommitted at rest: on the notebook it wrote 10 MB
        // of random pages to disk with the pool holding 140 MB.
        let budget = self.growth_budget();
        if budget == 0
            || self.active_count() <= INITIAL_DEVICES as usize
            || !self.writeback_usable
            || self.config.writeback_total == 0
        {
            return;
        }
        let free_slots = stats.total_disksize.saturating_sub(stats.total_orig_data);
        let unbackable = free_slots.saturating_sub(backable_bytes(budget, stats.total_phys_used));
        if unbackable == 0 || crate::is_dry_run() {
            return;
        }
        // A quarter of a normal pass per tick. Under the pressure that brings
        // this on, writeback crawls -- 256 MB took 70 s on the notebook -- and
        // the pass holds the monitor thread for as long as it runs.
        let targets = self.writeback_targets();
        self.write_back(
            &targets,
            "huge",
            unbackable
                .div_ceil(4096)
                .min(defaults::ZRAM_WRITEBACK_PAGES_PER_PASS / 4),
            stats.total_phys_used,
        );
    }

    /// Active devices that have a backing store, as (id, sysfs path).
    fn writeback_targets(&self) -> Vec<(u32, String)> {
        self.devices
            .iter()
            .filter(|d| d.state == ZramDeviceState::Active)
            .filter(|d| backing_dev_of(&d.sysfs_path).is_some())
            .map(|d| (d.id, d.sysfs_path.clone()))
            .collect()
    }

    /// Write up to `want` pages of the kind `mode` names to the backing stores
    /// and log what that returned to RAM.
    fn write_back(&self, targets: &[(u32, String)], mode: &str, want: u64, phys_before: u64) {
        // Live headroom under the pool's disk budget, in 4 KiB pages: the
        // configured total minus what the stores already hold. `bd_count` falls
        // as pages fault back, so this reopens without a restart. This is where
        // `writeback_total` is really enforced -- a store's file size is only a
        // ceiling, and the stores' sizes together can exceed the budget.
        let occupied_pages: u64 = targets.iter().map(|(_, s)| bd_count(s)).sum();
        let mut budget_pages = (self.config.writeback_total / 4096)
            .saturating_sub(occupied_pages)
            .min(want);
        let mut written: u64 = 0;

        for (id, sysfs) in targets {
            // Bound the write two ways. Per pass: never more than
            // PAGES_PER_PASS, or a first run on a rotational disk would hold the
            // monitor thread for minutes and delay shutdown with it. Per pool:
            // never past the budget headroom, so the stores together stay under
            // `writeback_total`. Once the budget is spent the rest waits for
            // pages to fault back and free it.
            let pass_limit = budget_pages.min(defaults::ZRAM_WRITEBACK_PAGES_PER_PASS);
            if pass_limit == 0 {
                break;
            }
            let _ = std::fs::write(format!("{}/writeback_limit_enable", sysfs), "1");
            let _ = std::fs::write(format!("{}/writeback_limit", sysfs), pass_limit.to_string());

            let start = bd_count(sysfs);
            if let Err(e) = std::fs::write(format!("{}/writeback", sysfs), mode) {
                // Nothing to recover from, and nothing is lost. The store sizes
                // its block bitmap from the file's *apparent* size, so a sparse
                // file on a filesystem that has filled up will hand out a block
                // and then fail the write. Read on both 6.12 and current: the
                // failing entry keeps its compressed copy and is never flagged
                // ZRAM_WB, so the page stays readable from RAM. The two differ
                // only in block accounting -- current releases the block, 6.12
                // carries it to the next iteration -- which costs nothing here.
                // Running out of blocks is the same story one step earlier:
                // -ENOSPC and the loop stops. Either way the pass degrades into
                // the behaviour we had before writeback existed, and a later
                // pass picks up once faults free blocks.
                debug!("ZramPool: zram{} writeback: {}", id, e);
            }
            let moved = bd_count(sysfs).saturating_sub(start);
            budget_pages = budget_pages.saturating_sub(moved);
            written += moved;
        }

        if written == 0 {
            return;
        }
        // Page count, then the RAM it actually returned. Deliberately no
        // bytes-written figure: that would mean multiplying by an assumed page
        // size, and this package builds for aarch64 too, where a 64 KiB-page
        // kernel would make the number silently wrong.
        let after = self
            .get_pool_stats()
            .map_or(phys_before, |s| s.total_phys_used);
        info!(
            "ZramPool: wrote back {} {} page(s); pool RAM {}MB -> {}MB",
            written,
            mode,
            phys_before / MB,
            after / MB
        );
    }

    /// Create a new ZRAM device and add it to the pool
    fn create_device(&mut self, disksize: u64) -> Result<()> {
        if self.active_count() >= self.config.max_devices as usize {
            return Err(ZramError::PoolMaxDevices);
        }

        // Every device comes from hot_add (Linux 3.15+). Missing means the
        // zram module is not loaded; pre-systemd-swap.service pulls in
        // modprobe@zram.service for that.
        if !Path::new(ZRAM_HOT_ADD).exists() {
            let detail = "hot_add control node missing (zram module not loaded)".to_string();
            error!("ZramPool: {}", detail);
            return Err(ZramError::ZramctlFailed(detail));
        }

        let new_id: u32 = read_file(ZRAM_HOT_ADD)?
            .trim()
            .parse()
            .map_err(|_| ZramError::ZramctlFailed("Invalid hot_add response".to_string()))?;

        let sysfs_path = format!("/sys/block/zram{}", new_id);
        let dev_path = format!("/dev/zram{}", new_id);

        // Set comp algorithm BEFORE disksize (kernel 6.1+ requires this order)
        let ctx = format!("ZramPool: zram{}", new_id);
        configure_zram_algorithm(&sysfs_path, &self.config.algorithm, &ctx);

        // Set algorithm_params before disksize for proper initialization
        if self.config.algorithm == "zstd" {
            let params_path = format!("{}/algorithm_params", sysfs_path);
            if Path::new(&params_path).exists() {
                let _ = std::fs::write(&params_path, "level=3");
            }
        }

        // Also before disksize: the kernel takes backing_dev only while the
        // device is still uninitialised, so a device that starts without one can
        // never be given writeback later. Failure here is not fatal -- the device
        // simply runs the way it always did.
        self.attach_backing_dev(&sysfs_path, new_id, disksize);

        // Write entries out still compressed, so the read path does not have to
        // decompress on the way back. Also init-time only, and absent before
        // 7.1, so a failure here just costs CPU on writeback reads.
        //
        // It does not shrink the backing store: each written-back entry still
        // reserves one 4 KiB block whatever its compressed size, so the disk
        // spent per MB of RAM freed stays at roughly the compression ratio --
        // measured 8.3 MB of disk for 2.8 MB of RAM. That ratio, not this
        // setting, is why the store has to be sized generously.
        let compressed_wb = format!("{}/compressed_writeback", sysfs_path);
        if Path::new(&compressed_wb).exists() {
            let _ = std::fs::write(&compressed_wb, "1");
        }

        // Set disksize
        let disksize_path = format!("{}/disksize", sysfs_path);
        if let Err(e) = std::fs::write(&disksize_path, disksize.to_string()) {
            error!("ZramPool: failed to set disksize for zram{}: {}", new_id, e);
            self.release_backing_store(&sysfs_path, new_id);
            let _ = std::fs::write(format!("{}/reset", sysfs_path), "1");
            return Err(ZramError::ZramctlFailed(
                "Failed to set disksize".to_string(),
            ));
        }

        // mkswap
        let mkswap_status = Command::new("mkswap")
            .arg(&dev_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;

        if !mkswap_status.success() {
            self.release_backing_store(&sysfs_path, new_id);
            let _ = std::fs::write(format!("{}/reset", sysfs_path), "1");
            return Err(ZramError::ZramctlFailed("mkswap failed".to_string()));
        }

        // One priority per device, in creation order. The kernel then fills the
        // first device before the next instead of spreading pages over all of
        // them, so the RAM split can favour the device that takes writes first,
        // and the last device -- the one contraction removes -- is written last.
        // Never below 0 unless the operator configured a negative priority:
        // swap files sit below 0 and must stay behind every zram device.
        let priority =
            (self.config.priority - self.devices.len() as i32).max(self.config.priority.min(0));
        let unit_name = gen_swap_unit(
            Path::new(&dev_path),
            Some(priority),
            Some("discard"),
            "zram",
        )?;

        activate_swap(&dev_path, Some(priority), true, &unit_name)?;

        let device = ZramDevice {
            id: new_id,
            disksize,
            sysfs_path,
            dev_path,
            unit_name,
            state: ZramDeviceState::Active,
            drain_attempts: 0,
        };

        info!(
            "ZramPool: zram{} created (disksize={}MB) — pool now has {} device(s)",
            new_id,
            disksize / (1024 * 1024),
            self.devices.len() + 1
        );

        self.devices.push(device);

        // Re-share the ceiling only once the device is really in the pool.
        //
        // `mem_limit` is per device, so the pool respects its ceiling only while
        // the shares add up to it — writing the new share to the newcomer alone
        // left earlier devices on the wider share they were created with, and
        // the total drifted 20% over at the fifth device and 66% at the eighth.
        //
        // But doing it before `mkswap`, `gen_swap_unit` and the two `systemctl`
        // calls meant a failure in any of them left the survivors narrowed for a
        // device that no longer exists. That was survivable when the pool had
        // four members and one-quarter went missing; with a single 15 GB device
        // and a 30 GB step, a `daemon-reload` timing out — the one thing
        // measured to take 1.4-1.9 s under memory pressure, which is exactly
        // when expansion runs — would leave it holding a third of the ceiling
        // against all of its slots, refusing writes with two-thirds free. That
        // is the failure this whole design removes, reintroduced by its own
        // error path.
        //
        // Nothing can land on the newcomer before `systemctl start` above, and
        // between that and this write it is briefly unlimited, which errs
        // toward using RAM rather than refusing it.
        self.share_mem_limit_across();

        Ok(())
    }

    /// Number of active (non-draining) devices
    fn active_count(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| d.state == ZramDeviceState::Active)
            .count()
    }

    /// Get aggregated stats from all active devices
    pub fn get_pool_stats(&self) -> Option<ZramPoolStats> {
        let mut total_disksize: u64 = 0;
        let mut total_orig: u64 = 0;
        let mut total_compr: u64 = 0;
        let mut total_phys: u64 = 0;
        let mut total_written_back: u64 = 0;
        let mut count: u8 = 0;

        for dev in &self.devices {
            if dev.state != ZramDeviceState::Active {
                continue;
            }
            if let Some(stats) = get_device_stats(&dev.sysfs_path, dev.disksize) {
                total_disksize += stats.disksize;
                total_orig += stats.orig_data_size;
                total_compr += stats.compr_data_size;
                total_phys += stats.mem_used_total;
                // `bd_count` is the pages currently parked on the backing
                // device, one 4 KiB block each. They still count in
                // `orig_data_size` but hold no RAM, so they must come out
                // before any RAM figure is derived from it.
                total_written_back += stats.written_back_bytes;
                count += 1;
            }
        }

        if count == 0 {
            return None;
        }

        let ratio = if total_compr > 0 {
            total_orig.saturating_sub(total_written_back) as f64 / total_compr as f64
        } else {
            0.0
        };

        let sizing_ratio = if total_phys > 0 {
            total_orig.saturating_sub(total_written_back) as f64 / total_phys as f64
        } else {
            0.0
        };

        let util = if total_disksize > 0 {
            ((total_orig as f64 / total_disksize as f64) * 100.0) as u8
        } else {
            0
        };

        let phys_pct = if self.ram_total > 0 {
            ((total_phys as f64 / self.ram_total as f64) * 100.0) as u8
        } else {
            0
        };

        Some(ZramPoolStats {
            device_count: count,
            total_disksize,
            total_orig_data: total_orig,
            total_phys_used: total_phys,
            compression_ratio: ratio,
            sizing_ratio,
            utilization_percent: util,
            phys_usage_percent: phys_pct,
        })
    }

    /// `disksize` for the next device, or 0 when no safe step is worth taking.
    ///
    /// The whole backable amount at once: the step is safe at any size, and on
    /// the notebook a step capped at half the budget arrived after the slots had
    /// run out, so a compressible load spilled to disk swap between expansions.
    fn next_device_disksize(&self, stats: &ZramPoolStats) -> u64 {
        expansion_step(
            self.growth_budget(),
            stats.total_phys_used,
            stats.total_disksize.saturating_sub(stats.total_orig_data),
            self.size_ceiling().saturating_sub(stats.total_disksize),
            self.ram_total * 5 / 100,
        )
    }

    fn should_expand(&self, stats: &ZramPoolStats) -> bool {
        // 1. Not at device limit
        if self.active_count() >= self.config.max_devices as usize {
            return false;
        }

        // 2. No draining devices (wait for cleanup to finish)
        if self
            .devices
            .iter()
            .any(|d| d.state == ZramDeviceState::Draining)
        {
            return false;
        }

        // 3. Slots running short. This is the only question
        // `utilization_percent` answers: it is `orig / disksize`, so it says
        // nothing whatever about how much RAM the pool is holding.
        if stats.utilization_percent < self.config.expand_threshold {
            return false;
        }

        // 4. The RAM compression has saved backs a step at 1:1.
        if self.next_device_disksize(stats) == 0 {
            return false;
        }

        // 5. Cooldown since last expansion
        if let Some(last) = self.last_expansion {
            if last.elapsed().as_secs() < self.config.expand_cooldown {
                return false;
            }
        }

        true
    }

    /// Expand the pool by adding a new ZRAM device
    fn expand(&mut self, stats: &ZramPoolStats) -> Result<()> {
        let disksize = self.next_device_disksize(stats);

        info!(
            "ZramPool: expanding — adding device (disksize={}MB, pool_util={}%, sizing_ratio={:.2}x, phys={}%)",
            disksize / (1024 * 1024),
            stats.utilization_percent,
            stats.sizing_ratio,
            stats.phys_usage_percent
        );

        self.create_device(disksize)?;
        self.last_expansion = Some(Instant::now());
        self.save_device_info()?;

        Ok(())
    }

    /// Report a pool that is out of RAM while still telling the kernel it has
    /// slots free.
    ///
    /// Each of those slots is a page the kernel will try to write and a device
    /// will refuse, while `should_reclaim_retry()` counts it as free swap rather
    /// than falling through to the disk tier — measured as 1.5 M suppressed
    /// `Write-error on swap-device` lines and a hang with no OOM kill. Sizing is
    /// meant to make it unreachable, so if this fires the sizing was wrong, and
    /// nothing else in the log would say so.
    ///
    /// Pool-wide, because `mem_limit_shares` lets every device take all the
    /// headroom the pool has left: the per-device limits bind when the pool
    /// total reaches the budget, not before.
    fn warn_on_unbackable_slots(&self, stats: &ZramPoolStats) {
        let budget = self.mem_limit_total();
        if budget == 0 || stats.total_phys_used * 100 < budget * 95 {
            return;
        }
        // How much more the pool could still take at the compression it is
        // achieving, versus how many slots it is advertising.
        let free_slots = stats.total_disksize.saturating_sub(stats.total_orig_data);
        let still_backable =
            (budget.saturating_sub(stats.total_phys_used) as f64 * stats.sizing_ratio) as u64;
        let unbackable = free_slots.saturating_sub(still_backable);
        // Only when the amount matters: a pool at its budget with a sliver
        // unbackable is simply full, and the kernel moves on to disk swap.
        if unbackable < 64 * MB {
            return;
        }
        warn!(
            "ZramPool: pool at its RAM budget ({}MB/{}MB) with {}MB of slots it cannot back",
            stats.total_phys_used / MB,
            budget / MB,
            unbackable / MB
        );
    }

    /// Check if pool should contract (remove last device)
    fn should_contract(&self, stats: &ZramPoolStats) -> bool {
        // 1. Never remove the first device. It is the one sized to the RAM
        // ceiling with no bet on compression, so it is the pool's floor; the
        // candidate below is always `devices.last()`, the lowest-priority
        // expansion device.
        if self.active_count() <= INITIAL_DEVICES as usize {
            return false;
        }
        let Some(last_dev) = self.devices.last() else {
            return false;
        };
        if last_dev.state != ZramDeviceState::Active {
            return false;
        }
        let Some(last) = get_device_stats(&last_dev.sysfs_path, last_dev.disksize) else {
            return false;
        };

        // 2. Either the pool advertises slots its RAM can no longer back, or it
        // has sat nearly idle.
        if !self.overcommitted(stats, &last) {
            if stats.utilization_percent > self.config.contract_threshold {
                return false;
            }
            if last.memory_utilization() > 5 {
                return false;
            }
            match self.low_util_since {
                Some(since) if since.elapsed().as_secs() >= self.config.contract_stability => {}
                _ => return false,
            }
        }

        // 5. Cooldown since last contraction
        if let Some(last) = self.last_contraction {
            if last.elapsed().as_secs() < 60 {
                return false;
            }
        }

        true
    }

    /// Whether the pool has more free slots than the RAM left under
    /// `growth_budget` can back at 1:1 -- churn is eating the reserve -- and
    /// the last device is cheap enough to empty back into RAM.
    ///
    /// Expansion never creates that state; it appears when compressible data
    /// leaves. Its slots come free but the RAM it saved goes with it, so a pool
    /// grown on 4x data and then emptied advertises far more than `mem_limit`,
    /// and an incompressible burst would meet the refusing-device hang that
    /// `expansion_step` exists to prevent. Removing the lowest-priority device
    /// takes its free slots away; it is also the device written last, so it is
    /// the one most likely to be nearly empty. `swapoff` reads its pages back
    /// into RAM, hence the requirement that they fit twice over.
    fn overcommitted(&self, stats: &ZramPoolStats, last: &ZramStats) -> bool {
        let budget = self.growth_budget();
        if budget == 0 {
            return false;
        }
        let free_slots = stats.total_disksize.saturating_sub(stats.total_orig_data);
        if free_slots <= backable_bytes(budget, stats.total_phys_used) {
            return false;
        }
        crate::meminfo::get_mem_stats(&["MemAvailable"])
            .ok()
            .and_then(|m| m.get("MemAvailable").copied())
            .is_some_and(|available| last.orig_data_size.saturating_mul(2) <= available)
    }

    /// Single non-blocking swapoff attempt for a device at the given index.
    /// On success, finalizes hot-remove and returns true.
    /// On failure, increments drain_attempts and returns false.
    fn try_drain_device(&mut self, idx: usize) -> Result<bool> {
        let dev_path = self.devices[idx].dev_path.clone();
        let dev_id = self.devices[idx].id;

        // The syscall wrapper, not the `swapoff` binary. Two reasons: this runs
        // while the pool is shedding a device, so spawning a process is the
        // least reliable thing to do just then; and the subprocess form threw
        // the reason away with `.map(|s| s.success())`, which is why a device
        // that refused to drain used to report five identical failures and no
        // cause. It also keeps every swapoff in the crate behind the single
        // audited `unsafe` block instead of a second, untyped path.
        let succeeded = match crate::systemd::swapoff(&dev_path) {
            Ok(()) => true,
            Err(e) => {
                debug!("ZramPool: swapoff {} failed: {}", dev_path, e);
                false
            }
        };

        if !succeeded {
            self.devices[idx].drain_attempts += 1;
            return Ok(false);
        }

        let sysfs_path = self.devices[idx].sysfs_path.clone();
        let unit_name = self.devices[idx].unit_name.clone();

        let _ = systemctl(SystemctlAction::Stop, &unit_name);
        // Before the reset, which clears backing_dev and would strand the loop.
        // swapoff already moved every page back into RAM or another swap device,
        // so nothing is lost with the file.
        self.release_backing_store(&sysfs_path, dev_id);
        if let Err(e) = reset_after_swapoff(&sysfs_path) {
            warn!("ZramPool: reset of zram{} failed: {}", dev_id, e);
        }
        if Path::new(ZRAM_HOT_REMOVE).exists() {
            let _ = std::fs::write(ZRAM_HOT_REMOVE, dev_id.to_string());
        }
        let unit_path = format!("/run/systemd/system/{}", unit_name);
        let _ = std::fs::remove_file(unit_path);
        let _ = systemctl(SystemctlAction::DaemonReload, "");

        self.devices.remove(idx);
        // Widen the survivors' share again, or the pool would stay capped at
        // the narrower share it held while the drained device was still up.
        self.share_mem_limit_across();
        self.last_contraction = Some(Instant::now());

        info!(
            "ZramPool: zram{} removed — pool now has {} device(s)",
            dev_id,
            self.devices.len()
        );
        self.save_device_info()?;
        Ok(true)
    }

    /// Retry a pending swapoff for a Draining device (called each monitor iteration).
    fn retry_draining(&mut self) -> Result<()> {
        const MAX_DRAIN_ATTEMPTS: u32 = 5;

        let Some(idx) = self
            .devices
            .iter()
            .position(|d| d.state == ZramDeviceState::Draining)
        else {
            return Ok(());
        };

        let dev_id = self.devices[idx].id;
        let attempts = self.devices[idx].drain_attempts;

        if attempts >= MAX_DRAIN_ATTEMPTS {
            warn!(
                "ZramPool: swapoff failed for zram{} after {} attempts, aborting contraction",
                dev_id, MAX_DRAIN_ATTEMPTS
            );
            self.devices[idx].state = ZramDeviceState::Active;
            self.devices[idx].drain_attempts = 0;
            self.last_contraction = Some(Instant::now());
            return Ok(());
        }

        self.try_drain_device(idx)?;
        Ok(())
    }

    /// Contract the pool by removing the last device
    fn contract(&mut self) -> Result<()> {
        if self.devices.len() <= 1 {
            return Ok(());
        }

        let last_idx = self.devices.len() - 1;
        let dev = &mut self.devices[last_idx];
        dev.state = ZramDeviceState::Draining;
        dev.drain_attempts = 0;

        info!(
            "ZramPool: contracting — removing zram{} (swapoff...)",
            dev.id
        );

        // First attempt; further retries handled non-blocking in retry_draining()
        self.try_drain_device(last_idx)?;
        Ok(())
    }

    /// Save device info for external consumers (swapfile manager, status command)
    fn save_device_info(&self) -> Result<()> {
        let active: Vec<String> = self
            .devices
            .iter()
            .filter(|d| d.state == ZramDeviceState::Active)
            .map(|d| format!("{}\n{}", d.dev_path, d.sysfs_path))
            .collect();

        let info = active.join("\n---\n");
        std::fs::write(format!("{}/zram/device", WORK_DIR), &info)?;

        Ok(())
    }

    /// Main monitoring loop — runs on dedicated thread
    pub fn run_monitor(&mut self) -> Result<()> {
        info!(
            "ZramPool: monitor started (max_devices={}, expand_threshold={}%, contract_threshold={}%)",
            self.config.max_devices,
            self.config.expand_threshold,
            self.config.contract_threshold
        );

        // Woken by memory pressure as soon as it starts, and otherwise every
        // `check_interval` for the slow trends pressure does not announce.
        let pressure = crate::meminfo::PressureWait::new();
        let check_interval = Duration::from_secs(self.config.check_interval);
        let mut last_log = Instant::now();

        loop {
            pressure.wait(check_interval);

            if crate::is_shutdown() {
                break;
            }

            let stats = match self.get_pool_stats() {
                Some(s) => s,
                None => continue,
            };

            // Periodic log (every ~30s)
            if last_log.elapsed() >= Duration::from_secs(30) {
                last_log = Instant::now();
                info!(
                    "ZramPool: {} dev(s), util={}%, ratio={:.2}x, phys={}% ({}MB/{}MB)",
                    stats.device_count,
                    stats.utilization_percent,
                    stats.compression_ratio,
                    stats.phys_usage_percent,
                    stats.total_phys_used / (1024 * 1024),
                    self.ram_total / (1024 * 1024)
                );

                self.warn_on_unbackable_slots(&stats);
            }

            // Track low utilization for contraction stability
            if stats.utilization_percent <= self.config.contract_threshold {
                if self.low_util_since.is_none() {
                    self.low_util_since = Some(Instant::now());
                }
            } else {
                self.low_util_since = None;
            }

            // Ahead of the expansion decision on purpose. Both read the same
            // `stats`, and a pass that lands lowers `phys` without touching
            // `orig`, so the utilisation the expansion decision reads is
            // unchanged -- writeback moves where a page is stored, not how many
            // slots the pool has spent. It does change `sizing_ratio`, which is
            // why that figure subtracts written-back pages.
            self.run_writeback(&stats);
            self.relieve_overcommit(&stats);

            // Re-share the ceiling by demand on every tick. `mem_limit` is one
            // of the few zram attributes writable at runtime, and demand moves:
            // pages land on the highest-priority device with free slots and
            // leave from any device, so free slots and the RAM that backs them
            // drift apart. Only worth doing with more than one device -- a
            // lone device already owns the ceiling.
            if self.devices.len() > 1 {
                self.share_mem_limit_across();
            }

            // Expansion decision
            if self.should_expand(&stats) {
                if let Err(e) = self.expand(&stats) {
                    warn!("ZramPool: expansion failed: {}", e);
                }
            }

            // Resume pending drain
            if let Err(e) = self.retry_draining() {
                warn!("ZramPool: drain retry failed: {}", e);
            }

            // Contraction decision
            if self.should_contract(&stats) {
                if let Err(e) = self.contract() {
                    warn!("ZramPool: contraction failed: {}", e);
                }
            }
        }

        Ok(())
    }
}

// =============================================================================
// Shared Types — ZramStats
// =============================================================================

/// Zram statistics for monitoring (per-device)
#[derive(Debug, Clone)]
pub struct ZramStats {
    pub orig_data_size: u64,
    /// Original bytes held on the backing device, not in the compressed RAM pool.
    pub written_back_bytes: u64,
    pub compr_data_size: u64,
    pub mem_used_total: u64,
    pub mem_limit: u64,
    pub disksize: u64,
}

impl ZramStats {
    pub fn compression_ratio(&self) -> f64 {
        if self.compr_data_size == 0 {
            0.0
        } else {
            self.orig_data_size.saturating_sub(self.written_back_bytes) as f64
                / self.compr_data_size as f64
        }
    }

    pub fn memory_utilization(&self) -> u8 {
        if self.disksize == 0 {
            0
        } else {
            ((self.orig_data_size as f64 / self.disksize as f64) * 100.0) as u8
        }
    }
}

/// Get aggregated zram stats from saved device info (for status command)
pub fn get_zram_stats() -> Option<ZramStats> {
    let device_info = format!("{}/zram/device", WORK_DIR);
    if !Path::new(&device_info).exists() {
        return None;
    }

    let info = std::fs::read_to_string(&device_info).ok()?;

    // New multi-device format: sections separated by "---"
    let sections: Vec<&str> = info.split("---").collect();
    let mut total_orig: u64 = 0;
    let mut total_compr: u64 = 0;
    let mut total_phys: u64 = 0;
    let mut total_disksize: u64 = 0;
    let mut mem_limit: u64 = 0;
    let mut total_written_back: u64 = 0;
    let mut found = false;

    for section in &sections {
        let lines: Vec<&str> = section.trim().lines().collect();
        if lines.len() < 2 {
            continue;
        }
        let sysfs = lines[1].trim();
        let disksize_path = format!("{}/disksize", sysfs);
        let disksize: u64 = std::fs::read_to_string(&disksize_path)
            .ok()?
            .trim()
            .parse()
            .ok()?;

        if let Some(stats) = get_device_stats(sysfs, disksize) {
            total_orig += stats.orig_data_size;
            total_compr += stats.compr_data_size;
            total_phys += stats.mem_used_total;
            total_disksize += stats.disksize;
            mem_limit += stats.mem_limit;
            total_written_back += stats.written_back_bytes;
            found = true;
        }
    }

    if !found {
        return None;
    }

    Some(ZramStats {
        orig_data_size: total_orig,
        written_back_bytes: total_written_back,
        compr_data_size: total_compr,
        mem_used_total: total_phys,
        mem_limit,
        disksize: total_disksize,
    })
}

/// Read stats for a specific ZRAM device by sysfs path
fn get_device_stats(sysfs_path: &str, disksize: u64) -> Option<ZramStats> {
    let mm_stat_path = format!("{}/mm_stat", sysfs_path);
    let mm_stat = std::fs::read_to_string(&mm_stat_path).ok()?;
    let fields: Vec<u64> = mm_stat
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();

    if fields.len() < 5 {
        return None;
    }

    Some(ZramStats {
        orig_data_size: fields[0],
        written_back_bytes: bd_count(sysfs_path) * BD_BLOCK_BYTES,
        compr_data_size: fields[1],
        mem_used_total: fields[2],
        mem_limit: fields[3],
        disksize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::GB;

    #[test]
    fn backing_dev_names_loop_by_node_in_any_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let sysfs = dir.path().to_str().unwrap();
        let file = dir.path().join("backing_dev");
        for (raw, want) in [
            ("/dev/loop3\n", Some("/dev/loop3")),
            ("/loop3\n", Some("/dev/loop3")),
            ("/dev/sda3\n", Some("/dev/sda3")),
            ("/loopback\n", Some("/loopback")),
            ("none\n", None),
        ] {
            std::fs::write(&file, raw).unwrap();
            assert_eq!(backing_dev_of(sysfs).as_deref(), want, "{raw:?}");
        }
    }

    // Private sysfs fixtures exercise the real redistribution path without
    // changing the host's swap devices. Sizes are expressed in kernel pages.
    fn quota_pool(rows: &[(u64, u64, u64)], budget: u64) -> (tempfile::TempDir, ZramPool) {
        let dir = tempfile::tempdir().unwrap();
        let page = crate::meminfo::get_page_size();
        let mut config = ZramPoolConfig::from_config(&cfg_from(&[]));
        config.mem_limit_bytes = budget * page;
        let devices = rows
            .iter()
            .enumerate()
            .map(|(id, &(orig, phys, disk))| {
                let path = dir.path().join(id.to_string());
                std::fs::create_dir(&path).unwrap();
                std::fs::write(
                    path.join("mm_stat"),
                    format!(
                        "{} {} {} {} 0",
                        orig * page,
                        phys * page,
                        phys * page,
                        budget * page
                    ),
                )
                .unwrap();
                std::fs::write(path.join("mem_limit"), (budget * page).to_string()).unwrap();
                ZramDevice {
                    id: id as u32,
                    disksize: disk * page,
                    sysfs_path: path.to_string_lossy().into_owned(),
                    dev_path: String::new(),
                    unit_name: String::new(),
                    state: ZramDeviceState::Active,
                    drain_attempts: 0,
                }
            })
            .collect();
        (
            dir,
            ZramPool {
                devices,
                config,
                ram_total: budget * page * 2,
                last_expansion: None,
                last_contraction: None,
                low_util_since: None,
                last_writeback: None,
                writeback_usable: true,
            },
        )
    }

    fn limits_in_pages(pool: &ZramPool) -> Vec<u64> {
        let page = crate::meminfo::get_page_size();
        pool.devices
            .iter()
            .map(|d| {
                read_file(format!("{}/mem_limit", d.sysfs_path))
                    .unwrap()
                    .trim()
                    .parse::<u64>()
                    .unwrap()
                    / page
            })
            .collect()
    }

    /// `quota_pool` with a writeback store on every device, so the fixture can
    /// see which `writeback` mode the pool asked for and how many pages.
    fn overcommit_pool(rows: &[(u64, u64, u64)], budget: u64) -> (tempfile::TempDir, ZramPool) {
        let (dir, mut pool) = quota_pool(rows, budget);
        pool.config.writeback_total = GB;
        for dev in &pool.devices {
            std::fs::write(format!("{}/backing_dev", dev.sysfs_path), "/dev/loop9\n").unwrap();
        }
        (dir, pool)
    }

    fn writeback_requests(pool: &ZramPool) -> Vec<Option<(String, String)>> {
        pool.devices
            .iter()
            .map(|d| {
                let mode = std::fs::read_to_string(format!("{}/writeback", d.sysfs_path)).ok()?;
                let limit =
                    std::fs::read_to_string(format!("{}/writeback_limit", d.sysfs_path)).ok()?;
                Some((mode, limit))
            })
            .collect()
    }

    #[test]
    fn overcommitted_pool_writes_huge_pages_back() {
        // 40000 slots, 2000 pages resident, growth budget 6500 pages: far more
        // free slots than RAM left to back them, so the per-tick cap binds.
        let (_dir, mut pool) = overcommit_pool(&[(1000, 1000, 20000), (1000, 1000, 20000)], 10000);
        let stats = pool.get_pool_stats().unwrap();
        pool.relieve_overcommit(&stats);
        let want = (defaults::ZRAM_WRITEBACK_PAGES_PER_PASS / 4).to_string();
        for request in writeback_requests(&pool) {
            assert_eq!(request, Some(("huge".to_string(), want.clone())));
        }
    }

    #[test]
    fn backable_or_lone_pool_writes_nothing_back() {
        // Free slots within what the growth budget still backs.
        let (_dir, mut pool) = overcommit_pool(&[(1000, 1000, 2000), (1000, 1000, 2000)], 10000);
        let stats = pool.get_pool_stats().unwrap();
        pool.relieve_overcommit(&stats);
        assert_eq!(writeback_requests(&pool), [None, None]);

        // A lone device is safe by construction, however it looks.
        let (_dir, mut pool) = overcommit_pool(&[(1000, 1000, 40000)], 10000);
        let stats = pool.get_pool_stats().unwrap();
        pool.relieve_overcommit(&stats);
        assert_eq!(writeback_requests(&pool), [None]);

        // No writeback budget configured.
        let (_dir, mut pool) = overcommit_pool(&[(1000, 1000, 20000), (1000, 1000, 20000)], 10000);
        pool.config.writeback_total = 0;
        let stats = pool.get_pool_stats().unwrap();
        pool.relieve_overcommit(&stats);
        assert_eq!(writeback_requests(&pool), [None, None]);
    }

    #[test]
    fn redistribution_lets_each_device_take_what_the_pool_has_left() {
        // 9000 of 10000 pages held: each device may add the same 1000.
        let (_dir, pool) = quota_pool(&[(8000, 8000, 9000), (1000, 1000, 91000)], 10000);
        pool.share_mem_limit_across();
        assert_eq!(limits_in_pages(&pool), [9000, 2000]);
    }

    #[test]
    fn redistribution_counts_devices_whose_swapoff_failed() {
        // A draining device still holds its RAM; ignoring it would hand the
        // first device 10000 pages.
        let (_dir, mut pool) = quota_pool(&[(8000, 8000, 9000), (1000, 1000, 91000)], 10000);
        pool.devices[1].state = ZramDeviceState::Draining;
        pool.share_mem_limit_across();
        assert_eq!(limits_in_pages(&pool)[0], 9000);
    }

    #[test]
    fn redistribution_does_not_increase_after_failed_decrease_or_missing_stats() {
        for missing_stats in [false, true] {
            let (dir, pool) = quota_pool(&[(100, 100, 1000), (100, 100, 1000)], 1000);
            let page = crate::meminfo::get_page_size();
            std::fs::write(
                dir.path().join("0/mm_stat"),
                format!(
                    "{} {} {} {} 0",
                    100 * page,
                    100 * page,
                    100 * page,
                    100 * page
                ),
            )
            .unwrap();
            std::fs::write(dir.path().join("0/mem_limit"), (100 * page).to_string()).unwrap();
            let failed = dir.path().join(if missing_stats {
                "1/mm_stat"
            } else {
                "1/mem_limit"
            });
            std::fs::remove_file(&failed).unwrap();
            std::fs::create_dir(&failed).unwrap();
            pool.share_mem_limit_across();
            assert_eq!(
                read_file(dir.path().join("0/mem_limit")).unwrap(),
                (100 * page).to_string()
            );
        }
    }

    #[test]
    fn ram_shares_stay_finite_and_bounded_with_empty_full_and_over_budget_devices() {
        for page in [4096, 65536] {
            for budget in [2, 3, 100, 1000, 10000] {
                for used in [0, 1, 80, 100, 10000] {
                    let members = [
                        stats(used * page, used * page, 10000 * page),
                        stats(0, 0, 10000 * page),
                    ];
                    let shares =
                        mem_limit_shares(&members, budget * page + page - 1, page).unwrap();
                    let case = format!("budget={budget}, used={used}, shares={shares:?}");
                    assert!(shares.iter().all(|&s| s >= page && s % page == 0), "{case}");
                    // No single device may ever pass the pool's ceiling.
                    assert!(shares.iter().all(|&s| s <= budget * page), "{case}");
                    if used < budget {
                        assert!(shares[0] >= used * page, "{case}");
                    } else {
                        // Over budget: nobody may grow.
                        assert!(shares[0] <= used * page, "{case}");
                    }
                }
            }
            assert!(mem_limit_shares(&[], page, page).is_none());
            assert!(mem_limit_shares(&[stats(0, 0, page); 1], page - 1, page).is_none());
        }
    }

    #[test]
    fn ram_shares_give_every_device_the_whole_headroom() {
        let mut first = stats(8 * MB, 2 * MB, 16 * MB);
        first.written_back_bytes = 4 * MB;
        let second = stats(4096, 3 * 4096, 8 * MB);
        let spare = 16 * MB - 2 * MB - 3 * 4096;
        let shares = mem_limit_shares(&[first.clone(), second], 16 * MB, 4096).unwrap();
        assert_eq!(shares, [2 * MB + spare, 3 * 4096 + spare]);
        first.written_back_bytes = first.orig_data_size + 4096;
        assert_eq!(first.compression_ratio(), 0.0);
    }

    #[test]
    fn full_pool_keeps_room_for_allocator_batches() {
        // At the ceiling, each device still gets 1 MiB for in-flight batches.
        let full = stats(943 * MB, 1885 * MB, 943 * MB);
        let newcomer = stats(4096, 4096, 943 * MB);
        let shares = mem_limit_shares(&[full, newcomer], 1886 * MB, 4096).unwrap();
        assert_eq!(shares[0], 1886 * MB, "shares={shares:?}");
        assert!(shares[1] >= 4096 + MB, "shares={shares:?}");
    }

    #[test]
    fn compression_ratio_excludes_written_back_pages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mm_stat"), "4096000 1024000 1024000 0 0").unwrap();
        std::fs::write(dir.path().join("bd_stat"), "500 0 500").unwrap();
        let stats = get_device_stats(dir.path().to_str().unwrap(), 8192000).unwrap();
        assert_eq!(stats.compression_ratio(), 2.0);
        assert_eq!(stats.memory_utilization(), 50);
    }

    fn stats(orig: u64, compr: u64, disk: u64) -> ZramStats {
        ZramStats {
            orig_data_size: orig,
            written_back_bytes: 0,
            compr_data_size: compr,
            mem_used_total: compr,
            mem_limit: 0,
            disksize: disk,
        }
    }

    // ── ZramStats::compression_ratio ─────────────────────────────────────────

    #[test]
    fn compression_ratio_zero_compr_returns_zero() {
        assert_eq!(stats(100, 0, 0).compression_ratio(), 0.0);
    }

    #[test]
    fn compression_ratio_4x() {
        let r = stats(400, 100, 0).compression_ratio();
        assert!((r - 4.0).abs() < 1e-9);
    }

    #[test]
    fn compression_ratio_1x_equal_data() {
        let r = stats(100, 100, 0).compression_ratio();
        assert!((r - 1.0).abs() < 1e-9);
    }

    // ── ZramStats::memory_utilization ────────────────────────────────────────

    #[test]
    fn memory_utilization_zero_disksize() {
        assert_eq!(stats(0, 0, 0).memory_utilization(), 0);
    }

    #[test]
    fn memory_utilization_half() {
        assert_eq!(stats(500, 100, 1000).memory_utilization(), 50);
    }

    #[test]
    fn memory_utilization_full() {
        assert_eq!(stats(1000, 200, 1000).memory_utilization(), 100);
    }

    // ── ZramPoolConfig::from_config clamps ───────────────────────────────────

    fn cfg_from(pairs: &[(&str, &str)]) -> Config {
        Config::from_pairs_for_tests(pairs.iter().map(|(k, v)| (*k, *v)))
    }

    #[test]
    fn pool_config_max_devices_clamps_to_1_8() {
        for v in ["8", "99"] {
            let cfg = cfg_from(&[("zram_max_devices", v)]);
            assert_eq!(ZramPoolConfig::from_config(&cfg).max_devices, 8);
        }

        let cfg = cfg_from(&[("zram_max_devices", "0")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).max_devices, 1);
    }

    #[test]
    fn pool_config_expand_threshold_clamps_to_50_95() {
        let cfg = cfg_from(&[("zram_expand_threshold", "10")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).expand_threshold, 50);

        let cfg = cfg_from(&[("zram_expand_threshold", "200")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).expand_threshold, 95);
    }

    #[test]
    fn pool_config_contract_threshold_clamps_to_5_50() {
        let cfg = cfg_from(&[("zram_contract_threshold", "1")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).contract_threshold, 5);

        let cfg = cfg_from(&[("zram_contract_threshold", "90")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).contract_threshold, 50);
    }

    #[test]
    fn pool_config_check_interval_clamps_to_3_300() {
        let cfg = cfg_from(&[("zram_check_interval", "1")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).check_interval, 3);

        let cfg = cfg_from(&[("zram_check_interval", "9999")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).check_interval, 300);
    }

    #[test]
    fn pool_config_contract_stability_clamps_to_30_600() {
        let cfg = cfg_from(&[("zram_contract_stability", "1")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).contract_stability, 30);

        let cfg = cfg_from(&[("zram_contract_stability", "99999")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).contract_stability, 600);
    }

    #[test]
    fn pool_config_defaults_when_unset() {
        let cfg = cfg_from(&[]);
        let pc = ZramPoolConfig::from_config(&cfg);
        assert_eq!(pc.max_devices, defaults::ZRAM_MAX_DEVICES);
        assert_eq!(pc.algorithm, defaults::ZRAM_ALG);
        assert_eq!(pc.priority, defaults::ZRAM_PRIO);
    }

    #[test]
    fn pool_config_initial_size_percent_raw() {
        // from_config alone reports raw value; ZramPool::new enforces >=50.
        let cfg = cfg_from(&[("zram_size", "10%")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).size_ceiling_percent, 10);
    }

    #[test]
    fn pool_config_mem_limit_from_percent_string() {
        let ram = crate::meminfo::get_ram_size().unwrap();
        let cfg = cfg_from(&[("zram_mem_limit", "40%")]);
        assert_eq!(
            ZramPoolConfig::from_config(&cfg).mem_limit_bytes,
            ram * 40 / 100
        );
    }

    // An absolute ceiling used to parse as 0, which reads as "no limit" and now
    // also drops the pool back to advertising `zram_size` unbacked.
    #[test]
    fn pool_config_mem_limit_accepts_an_absolute_size() {
        let cfg = cfg_from(&[("zram_mem_limit", "512M")]);
        assert_eq!(
            ZramPoolConfig::from_config(&cfg).mem_limit_bytes,
            512 * 1024 * 1024
        );
    }

    #[test]
    fn pool_config_mem_limit_zero_means_no_ceiling() {
        let cfg = cfg_from(&[("zram_mem_limit", "0")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).mem_limit_bytes, 0);
    }

    #[test]
    fn pool_config_custom_algorithm() {
        let cfg = cfg_from(&[("zram_alg", "lz4")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).algorithm, "lz4");
    }

    // ── Writeback config ─────────────────────────────────────────────────────

    #[test]
    fn pool_config_writeback_path_follows_swapfile_path() {
        // Moving the swap files has to move the backing files with them, or the
        // new location would be outside the unit's ReadWritePaths.
        let cfg = cfg_from(&[("swapfile_path", "/mnt/big/swap")]);
        assert_eq!(
            ZramPoolConfig::from_config(&cfg).writeback_path,
            PathBuf::from("/mnt/big/swap")
        );
    }

    #[test]
    fn pool_config_writeback_path_defaults_to_swapfile_default() {
        let cfg = cfg_from(&[]);
        assert_eq!(
            ZramPoolConfig::from_config(&cfg).writeback_path,
            PathBuf::from(defaults::SWAPFILE_PATH)
        );
    }

    #[test]
    fn pool_config_writeback_path_strips_trailing_slash() {
        let cfg = cfg_from(&[("zram_writeback_path", "/mnt/big/swap/")]);
        assert_eq!(
            ZramPoolConfig::from_config(&cfg).writeback_path,
            PathBuf::from("/mnt/big/swap")
        );
    }

    // deny-path: the value reaches `btrfs subvolume create`, `chattr` and a
    // sweep that deletes files, so a system directory has to be refused rather
    // than acted on. Empty is the disabled sentinel writeback fails closed on.
    #[test]
    fn pool_config_writeback_path_rejects_system_dirs() {
        for bad in [
            "/etc", "/usr", "/usr/lib", "/sys", "/proc", "/dev", "/boot", "/bin", "/run",
        ] {
            let cfg = cfg_from(&[("zram_writeback_path", bad)]);
            assert_eq!(
                ZramPoolConfig::from_config(&cfg).writeback_path,
                PathBuf::new(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn pool_config_writeback_path_rejects_relative() {
        let cfg = cfg_from(&[("zram_writeback_path", "relative/dir")]);
        assert_eq!(
            ZramPoolConfig::from_config(&cfg).writeback_path,
            PathBuf::new()
        );
    }

    // allow-path: the locations an administrator legitimately picks.
    #[test]
    fn pool_config_writeback_path_accepts_writable_locations() {
        for good in [
            "/swapfile",
            "/var/lib/systemd-swap",
            "/mnt/disk2/swap",
            "/home/swap",
        ] {
            let cfg = cfg_from(&[("zram_writeback_path", good)]);
            assert_eq!(
                ZramPoolConfig::from_config(&cfg).writeback_path,
                PathBuf::from(good),
                "{good} must be accepted"
            );
        }
    }

    #[test]
    fn pool_config_writeback_path_honours_override() {
        let cfg = cfg_from(&[("zram_writeback_path", "/mnt/spare/wb")]);
        assert_eq!(
            ZramPoolConfig::from_config(&cfg).writeback_path,
            PathBuf::from("/mnt/spare/wb")
        );
    }

    #[test]
    fn pool_config_writeback_size_absolute() {
        let cfg = cfg_from(&[("zram_writeback_size", "4G")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_total, 4 * GB);
    }

    #[test]
    fn pool_config_writeback_size_zero_disables() {
        let cfg = cfg_from(&[("zram_writeback_size", "0")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_total, 0);
    }

    #[test]
    fn pool_config_writeback_size_garbage_disables() {
        // parse_size failing must not silently become a large default.
        let cfg = cfg_from(&[("zram_writeback_size", "banana")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_total, 0);
    }

    #[test]
    fn pool_config_writeback_threshold_clamps_to_5_90() {
        let cfg = cfg_from(&[("zram_writeback_threshold", "0")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_threshold, 5);

        let cfg = cfg_from(&[("zram_writeback_threshold", "200")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_threshold, 90);
    }

    #[test]
    fn pool_config_writeback_idle_clamps_to_60_86400() {
        // A one-second idle age would write the live working set to disk.
        let cfg = cfg_from(&[("zram_writeback_idle", "1")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_idle, 60);

        let cfg = cfg_from(&[("zram_writeback_idle", "999999")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_idle, 86400);
    }

    #[test]
    fn pool_config_writeback_interval_clamps_to_30_3600() {
        let cfg = cfg_from(&[("zram_writeback_interval", "1")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_interval, 30);

        let cfg = cfg_from(&[("zram_writeback_interval", "99999")]);
        assert_eq!(ZramPoolConfig::from_config(&cfg).writeback_interval, 3600);
    }

    #[test]
    fn pool_config_writeback_defaults_when_unset() {
        let pc = ZramPoolConfig::from_config(&cfg_from(&[]));
        assert_eq!(pc.writeback_threshold, defaults::ZRAM_WRITEBACK_THRESHOLD);
        assert_eq!(pc.writeback_idle, defaults::ZRAM_WRITEBACK_IDLE);
        assert_eq!(pc.writeback_interval, defaults::ZRAM_WRITEBACK_INTERVAL);
    }

    // ── Sizing: what the pool may advertise ──────────────────────────────────
    #[test]
    fn first_device_is_the_ram_ceiling_less_allocator_margin() {
        assert_eq!(expansion_step(2 * GB, 0, 0, 6 * GB, 0), 2 * GB * 95 / 100);
        assert_eq!(
            expansion_step(u64::MAX, 0, 0, u64::MAX, 0),
            (u64::MAX as u128 * 95 / 100) as u64
        );
    }

    #[test]
    fn expansion_never_exceeds_the_configured_ceiling() {
        assert_eq!(expansion_step(8 * GB, 0, 0, 6 * GB, 0), 6 * GB);
        assert_eq!(expansion_step(2 * GB, 0, 0, 0, 0), 0);
    }

    #[test]
    fn expansion_without_a_ram_ceiling_leaves_zram_size_alone() {
        // `zram_mem_limit=0` is the operator saying "no ceiling"; there is then
        // nothing to derive from and their number must survive untouched.
        assert_eq!(expansion_step(0, 0, 0, 6 * GB, 0), 6 * GB);
    }

    #[test]
    fn expansion_step_spends_only_ram_that_compression_saved() {
        // 16 GB budget, 4 GB of RAM holding the pool, 2 GB of slots still free:
        // 0.95 * 12 GB can back new slots, less the 2 GB already advertised.
        let step = expansion_step(16 * GB, 4 * GB, 2 * GB, 64 * GB, GB);
        assert_eq!(step, (12 * GB as u128 * 95 / 100) as u64 - 2 * GB);
    }

    #[test]
    fn expansion_step_refuses_a_step_too_small_to_pay_for_itself() {
        assert_eq!(expansion_step(8 * GB, 7 * GB, 0, 64 * GB, GB), 0);
    }

    #[test]
    fn expansion_step_refuses_when_free_slots_already_use_the_budget() {
        // Free slots beyond what RAM can back: the answer is 0, not a negative
        // that would wrap into an enormous device.
        assert_eq!(expansion_step(4 * GB, GB, 8 * GB, 64 * GB, 0), 0);
    }

    // Drive the sizing rule the way the monitor does: store data at `ratio`,
    // expand at 85% of slots, and stop once `until` is stored or the slots run
    // out. MB-granular to keep the arithmetic readable.
    struct Pool {
        disk: u64,
        orig: u64,
        phys: u64,
    }

    fn fill(pool: &mut Pool, ratio: u64, until: u64, limit: u64, ceiling: u64) {
        let Pool { disk, orig, phys } = pool;
        while *orig < (*disk).min(until) {
            *orig += MB;
            *phys += MB / ratio;
            assert!(
                *phys <= limit,
                "RAM ceiling reached with {} MB of slots free",
                disk.saturating_sub(*orig) / MB
            );
            if *orig * 100 >= *disk * 85 {
                *disk += expansion_step(
                    (limit as u128 * 65 / 100) as u64,
                    *phys,
                    disk.saturating_sub(*orig),
                    ceiling - *disk,
                    256 * MB,
                );
            }
        }
    }

    #[test]
    fn compressible_data_grows_the_pool_and_incompressible_data_stays_backable() {
        // The reference host: 31 GB of RAM, 50% budget, 150% ceiling.
        let (limit, ceiling) = (15 * GB + 800 * MB, 47 * GB);
        let fresh = || Pool {
            disk: expansion_step(limit, 0, 0, ceiling, 0),
            orig: 0,
            phys: 0,
        };

        // At the 4x the host measures, the pool grows to ~41 GB instead of
        // stopping at the 15 GB the RAM budget holds at 1:1.
        let mut pool = fresh();
        fill(&mut pool, 4, u64::MAX, limit, ceiling);
        assert!(pool.disk > 40 * GB, "disk={} MB", pool.disk / MB);
        assert!(
            pool.phys <= pool.disk / 4 + MB,
            "phys={} MB",
            pool.phys / MB
        );

        // Fed random data after compressible data, at any point in the growth:
        // every slot the pool advertised must still fit in RAM (asserted in
        // `fill`), and the pool keeps what the compressible part earned.
        for switch_at in [GB, 8 * GB, 14 * GB, 20 * GB, 30 * GB, 40 * GB] {
            let mut pool = fresh();
            fill(&mut pool, 4, switch_at, limit, ceiling);
            fill(&mut pool, 1, u64::MAX, limit, ceiling);
            assert!(pool.orig >= pool.disk);
        }
    }

    // ── writeback_share ──────────────────────────────────────────────────────
    //
    // Three bounds, and each test pins the one that is meant to bind.

    #[test]
    fn writeback_share_bounded_by_configured_total() {
        // Roomy disk, device larger than the total → the total decides.
        assert_eq!(writeback_share(16 * GB, 1024 * GB, 32 * GB), 16 * GB);
    }

    #[test]
    fn writeback_share_bounded_by_device_capacity() {
        // A store bigger than the device backing it can never fill: every entry
        // in it came out of that device.
        assert_eq!(writeback_share(16 * GB, 1024 * GB, 4 * GB), 4 * GB);
        // 4 GB RAM at the 50% default: a 2 GiB ceiling and a device sized to it.
        assert_eq!(writeback_share(2 * GB, 1024 * GB, 2 * GB), 2 * GB);
    }

    #[test]
    fn writeback_share_capped_by_free_disk() {
        // 16 GiB asked for, 8 GiB free → never claim more than a quarter of it.
        assert_eq!(writeback_share(16 * GB, 8 * GB, 32 * GB), 2 * GB);
    }

    #[test]
    fn writeback_share_gives_up_when_disk_is_tight() {
        // 768 MiB claimable of 3 GiB free is above the floor; 128 MiB is not.
        assert_eq!(writeback_share(16 * GB, 3 * GB, 32 * GB), 768 * MB);
        assert_eq!(writeback_share(16 * GB, 512 * MB, 32 * GB), 0);
        assert_eq!(writeback_share(16 * GB, 0, 32 * GB), 0);
    }

    #[test]
    fn writeback_share_below_floor_is_disabled() {
        // Exactly the floor is kept; just under it is not worth a loop and a
        // file, and a zero total is writeback switched off.
        assert_eq!(writeback_share(256 * MB, 1024 * GB, 32 * GB), 256 * MB);
        assert_eq!(writeback_share(256 * MB - 4 * MB, 1024 * GB, 32 * GB), 0);
        assert_eq!(writeback_share(0, 1024 * GB, 32 * GB), 0);
    }
}
