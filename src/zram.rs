// Zram configuration for systemd-swap
// Dynamic multi-ZRAM pool with adaptive expansion/contraction
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::config::{Config, WORK_DIR};
use crate::defaults;
use crate::helpers::{makedirs, parse_size, read_file};
use crate::systemd::{gen_swap_unit, systemctl, SystemctlAction};
use crate::{error, info, warn};

const ZRAM_MODULE: &str = "/sys/module/zram";
const ZRAM_HOT_ADD: &str = "/sys/class/zram-control/hot_add";
const ZRAM_HOT_REMOVE: &str = "/sys/class/zram-control/hot_remove";

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
    #[error("No free zram device found")]
    NoFreeDevice,
    #[error("Device busy after retries")]
    DeviceBusy,
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

/// Start zram swap
pub fn start(config: &Config) -> Result<()> {
    crate::systemd::notify_status("Setting up Zram...");

    info!("Zram: check module availability");
    if !is_available() {
        return Err(ZramError::NotAvailable);
    }
    info!("Zram: module found!");

    makedirs(format!("{}/zram", WORK_DIR))?;

    // Parse config values
    let zram_size = parse_size(config.get("zram_size").unwrap_or(defaults::ZRAM_SIZE)).map_err(ZramError::ZramctlFailed)?;
    let zram_alg = config.get("zram_alg").unwrap_or(defaults::ZRAM_ALG);
    let zram_prio: i32 = config.get_as("zram_prio").unwrap_or(defaults::ZRAM_PRIO);

    let zram_mem_limit = config
        .get_opt("zram_mem_limit")
        .and_then(|s| parse_size(s).ok())
        .unwrap_or(0);

    if zram_size == 0 {
        warn!("Zram: size is 0, skipping");
        return Ok(());
    }

    info!(
        "Zram: size = {} bytes ({} MiB)",
        zram_size,
        zram_size / (1024 * 1024)
    );

    info!("Zram: trying to initialize free device");
    if !Path::new(ZRAM_HOT_ADD).exists() {
        return Err(ZramError::NoFreeDevice);
    }
    let new_id: String = read_file(ZRAM_HOT_ADD)?.trim().to_string();
    let zram_dev = format!("/dev/zram{}", new_id);
    let zram_sysfs = format!("/sys/block/zram{}", new_id);
    info!("Zram: initialized: {}", zram_dev);

    configure_zram_algorithm(&zram_sysfs, zram_alg, "Zram");

    let disksize_path = format!("{}/disksize", zram_sysfs);
    if let Err(e) = std::fs::write(&disksize_path, zram_size.to_string()) {
        error!("Zram: failed to set disksize: {}", e);
        let _ = std::fs::write(format!("{}/reset", zram_sysfs), "1");
        return Err(ZramError::ZramctlFailed(
            "Failed to set disksize".to_string(),
        ));
    }

    if zram_mem_limit > 0 {
        let mem_limit_path = format!("{}/mem_limit", zram_sysfs);
        if Path::new(&mem_limit_path).exists() {
            match std::fs::write(&mem_limit_path, zram_mem_limit.to_string()) {
                Ok(_) => info!(
                    "Zram: mem_limit = {} MiB (RAM protection)",
                    zram_mem_limit / (1024 * 1024)
                ),
                Err(e) => warn!("Zram: failed to set mem_limit: {}", e),
            }
        }
    }

    // Run mkswap
    let mkswap_status = Command::new("mkswap")
        .arg(&zram_dev)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if !mkswap_status.success() {
        // Clean up the zram device on mkswap failure
        let zram_id = zram_dev.trim_start_matches("/dev/zram");
        let _ = std::fs::write(format!("/sys/block/zram{}/reset", zram_id), "1");
        return Err(ZramError::ZramctlFailed("mkswap failed".to_string()));
    }

    // Generate and start swap unit
    let unit_name = gen_swap_unit(
        Path::new(&zram_dev),
        Some(zram_prio),
        Some("discard"),
        "zram",
    )?;

    systemctl(SystemctlAction::DaemonReload, "")?;
    systemctl(SystemctlAction::Start, &unit_name)?;

    // Save zram info for status queries
    let zram_id = zram_dev.trim_start_matches("/dev/zram");
    let zram_sysfs = format!("/sys/block/zram{}", zram_id);
    let zram_info = format!("{}\n{}", zram_dev, zram_sysfs);
    let _ = std::fs::write(format!("{}/zram/device", WORK_DIR), &zram_info);

    crate::systemd::notify_status("Zram setup finished");
    Ok(())
}

/// Release a zram device
pub fn release(device: &str) -> Result<()> {
    let status = Command::new("zramctl")
        .args(["-r", device])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if !status.success() {
        return Err(ZramError::ZramctlFailed(format!(
            "Failed to release {}",
            device
        )));
    }

    // Hot-remove the device from the kernel to avoid orphaned zram entries
    let dev_id = device.trim_start_matches("/dev/zram");
    if Path::new(ZRAM_HOT_REMOVE).exists() {
        let _ = std::fs::write(ZRAM_HOT_REMOVE, dev_id);
    }

    Ok(())
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
    pub total_compr_data: u64,
    pub total_phys_used: u64,
    pub compression_ratio: f64,
    pub utilization_percent: u8,
    pub phys_usage_percent: u8,
    pub total_same_pages: u64,
    pub total_pages_compacted: u64,
}

/// Configuration for the ZramPool
#[derive(Debug, Clone)]
pub struct ZramPoolConfig {
    /// Maximum number of ZRAM devices (1-8)
    pub max_devices: u8,
    /// Initial ZRAM size as percentage of RAM (first device)
    pub initial_size_percent: u32,
    /// Compression algorithm
    pub algorithm: String,
    /// Swap priority (all devices same = round-robin)
    pub priority: i32,
    /// Minimum compression ratio to allow pool expansion
    pub expand_min_ratio: f64,
    /// Per-device mem_limit as percentage of RAM (0 = unlimited)
    pub mem_limit_percent: u32,
    /// Pool utilization % that triggers expansion
    pub expand_threshold: u8,
    /// Pool utilization % below which to contract
    pub contract_threshold: u8,
    /// Seconds between expansion attempts
    pub expand_cooldown: u64,
    /// Seconds utilization must stay low before contraction
    pub contract_stability: u64,
    /// Minimum free RAM % required for expansion
    pub min_free_ram_percent: u8,
    /// Seconds between monitor checks
    pub check_interval: u64,
}

impl ZramPoolConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_devices: config
                .get_as::<u8>("zram_max_devices")
                .unwrap_or(defaults::ZRAM_MAX_DEVICES)
                .clamp(1, 8),
            initial_size_percent: config
                .get_opt("zram_size")
                .and_then(|s| s.strip_suffix('%'))
                .and_then(|s| s.parse().ok())
                .unwrap_or(50),
            algorithm: config.get("zram_alg").unwrap_or(defaults::ZRAM_ALG).to_string(),
            priority: config.get_as("zram_prio").unwrap_or(defaults::ZRAM_PRIO),
            expand_min_ratio: config
                .get_as::<f64>("zram_expand_min_ratio")
                .unwrap_or(defaults::ZRAM_EXPAND_MIN_RATIO)
                .clamp(1.5, 5.0),
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
            min_free_ram_percent: config
                .get_as::<u8>("zram_min_free_ram")
                .unwrap_or(defaults::ZRAM_MIN_FREE_RAM)
                .clamp(5, 40),
            check_interval: config
                .get_as::<u64>("zram_check_interval")
                .unwrap_or(defaults::ZRAM_CHECK_INTERVAL)
                .clamp(3, 300),
            mem_limit_percent: config
                .get_opt("zram_mem_limit")
                .and_then(|s| s.strip_suffix('%'))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
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

        // Enforce minimum initial_size_percent
        if pool_config.initial_size_percent < 50 {
            pool_config.initial_size_percent = 50;
        }

        makedirs(format!("{}/zram", WORK_DIR))?;

        Ok(Self {
            devices: Vec::new(),
            config: pool_config,
            ram_total,
            last_expansion: None,
            last_contraction: None,
            low_util_since: None,
        })
    }

    /// Start the initial ZRAM devices (4 equal-sized devices for better distribution).
    /// If existing devices are found (e.g., from a previous instance that wasn't
    /// cleanly stopped), adopt them instead of creating new ones.
    pub fn start_primary(&mut self) -> Result<()> {
        crate::systemd::notify_status("Setting up ZramPool...");

        let total_disksize = self.ram_total * self.config.initial_size_percent as u64 / 100;
        if total_disksize == 0 {
            warn!("ZramPool: calculated disksize is 0, skipping");
            return Ok(());
        }

        const INITIAL_DEVICES: u32 = 4;
        let per_device_size = total_disksize / INITIAL_DEVICES as u64;

        // Try to adopt existing active zram swap devices first
        let adopted = self.adopt_existing_devices();
        if adopted > 0 {
            info!(
                "ZramPool: adopted {} existing device(s), need {} total",
                adopted, INITIAL_DEVICES
            );
        }

        let remaining = (INITIAL_DEVICES as usize).saturating_sub(self.devices.len());
        if remaining > 0 {
            info!(
                "ZramPool: creating {} new device(s) ({}MB each, alg={}, max_devices={})",
                remaining,
                per_device_size / (1024 * 1024),
                self.config.algorithm,
                self.config.max_devices
            );
            for _ in 0..remaining {
                self.create_device(per_device_size)?;
            }
        }

        self.save_device_info()?;
        crate::systemd::notify_status("ZramPool: initial devices ready");
        Ok(())
    }

    /// Adopt existing active zram swap devices from a previous instance.
    /// Returns the number of devices adopted.
    fn adopt_existing_devices(&mut self) -> usize {
        let mut adopted = 0;
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

            // Check if it's an active swap device via /proc/swaps
            let Ok(swaps) = std::fs::read_to_string("/proc/swaps") else {
                continue;
            };
            if !swaps.contains(&dev_path) {
                continue;
            }

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
            self.devices.push(device);
            adopted += 1;
        }
        adopted
    }

    /// Create a new ZRAM device and add it to the pool
    fn create_device(&mut self, disksize: u64) -> Result<()> {
        if self.active_count() >= self.config.max_devices as usize {
            return Err(ZramError::PoolMaxDevices);
        }

        if !Path::new(ZRAM_HOT_ADD).exists() {
            return Err(ZramError::ZramctlFailed(
                "Kernel doesn't support hot_add".to_string(),
            ));
        }

        let new_id: u32 = read_file(ZRAM_HOT_ADD)?
            .trim()
            .parse()
            .map_err(|_| ZramError::ZramctlFailed("Invalid hot_add response".to_string()))?;

        let sysfs_path = format!("/sys/block/zram{}", new_id);
        let dev_path = format!("/dev/zram{}", new_id);

        // Set comp algorithm BEFORE disksize (kernel 6.1+ requires this order)
        let ctx = format!("ZramPool: zram{}", new_id);
        configure_zram_algorithm(
            &sysfs_path,
            &self.config.algorithm,
            &ctx,
        );

        // Set algorithm_params before disksize for proper initialization
        if self.config.algorithm == "zstd" {
            let params_path = format!("{}/algorithm_params", sysfs_path);
            if Path::new(&params_path).exists() {
                let _ = std::fs::write(&params_path, "level=3");
            }
        }

        // Set disksize
        let disksize_path = format!("{}/disksize", sysfs_path);
        if let Err(e) = std::fs::write(&disksize_path, disksize.to_string()) {
            error!("ZramPool: failed to set disksize for zram{}: {}", new_id, e);
            let _ = std::fs::write(format!("{}/reset", sysfs_path), "1");
            return Err(ZramError::ZramctlFailed(
                "Failed to set disksize".to_string(),
            ));
        }

        // Per-device mem_limit: caps physical RAM usage per device
        if self.config.mem_limit_percent > 0 {
            let total_limit = self.ram_total * self.config.mem_limit_percent as u64 / 100;
            let device_count = (self.devices.len() as u64 + 1).max(4);
            let per_device_limit = total_limit / device_count;
            let mem_limit_path = format!("{}/mem_limit", sysfs_path);
            if Path::new(&mem_limit_path).exists() {
                match std::fs::write(&mem_limit_path, per_device_limit.to_string()) {
                    Ok(_) => info!(
                        "ZramPool: zram{} mem_limit = {}MB",
                        new_id,
                        per_device_limit / (1024 * 1024)
                    ),
                    Err(e) => warn!("ZramPool: failed to set mem_limit for zram{}: {}", new_id, e),
                }
            }
        }

        // mkswap
        let mkswap_status = Command::new("mkswap")
            .arg(&dev_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;

        if !mkswap_status.success() {
            let _ = std::fs::write(format!("{}/reset", sysfs_path), "1");
            return Err(ZramError::ZramctlFailed("mkswap failed".to_string()));
        }

        // Generate systemd swap unit and activate
        let unit_name = gen_swap_unit(
            Path::new(&dev_path),
            Some(self.config.priority),
            Some("discard"),
            "zram",
        )?;

        systemctl(SystemctlAction::DaemonReload, "")?;
        systemctl(SystemctlAction::Start, &unit_name)?;

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
        let mut total_same: u64 = 0;
        let mut total_compacted: u64 = 0;
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
                total_same += stats.same_pages;
                total_compacted += stats.pages_compacted;
                count += 1;
            }
        }

        if count == 0 {
            return None;
        }

        let ratio = if total_compr > 0 {
            total_orig as f64 / total_compr as f64
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
            total_compr_data: total_compr,
            total_phys_used: total_phys,
            compression_ratio: ratio,
            utilization_percent: util,
            phys_usage_percent: phys_pct,
            total_same_pages: total_same,
            total_pages_compacted: total_compacted,
        })
    }

    /// Calculate disksize for the next device
    fn calculate_next_disksize(&self, _stats: &ZramPoolStats) -> u64 {
        // Expansion devices use the same per-device size as initial ones
        let total_disksize = self.ram_total * self.config.initial_size_percent as u64 / 100;
        let min_size = self.ram_total * 5 / 100;
        (total_disksize / 4).max(min_size)
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

        // 3. Pool utilization above threshold
        if stats.utilization_percent < self.config.expand_threshold {
            return false;
        }

        // 4. Compression ratio good enough
        if stats.compression_ratio < self.config.expand_min_ratio {
            info!(
                "ZramPool: expansion skipped — ratio {:.2}x < min {:.1}x (data too incompressible)",
                stats.compression_ratio, self.config.expand_min_ratio
            );
            return false;
        }

        // 5. Enough free RAM (adaptive: higher ratio = lower minimum needed)
        // When compression is good, expanding ZRAM is better than letting
        // pages spill to slow disk swap — ZRAM is ~100x faster than HDD.
        if let Ok(free) = crate::meminfo::get_free_ram_percent() {
            let adaptive_min = if stats.compression_ratio >= 10.0 {
                2_u8 // Excellent: 2% free RAM is enough
            } else if stats.compression_ratio >= 5.0 {
                3_u8 // Very good: 3%
            } else if stats.compression_ratio >= 3.0 {
                5_u8 // Good: 5%
            } else if stats.compression_ratio >= 2.0 {
                8_u8 // Moderate: 8%
            } else {
                self.config.min_free_ram_percent // Poor: full threshold
            };
            if free < adaptive_min {
                info!(
                    "ZramPool: expansion skipped — free RAM {}% < min {}% (ratio {:.1}x)",
                    free, adaptive_min, stats.compression_ratio
                );
                return false;
            }
        }

        // 7. Cooldown since last expansion
        if let Some(last) = self.last_expansion {
            if last.elapsed().as_secs() < self.config.expand_cooldown {
                return false;
            }
        }

        true
    }

    /// Expand the pool by adding a new ZRAM device
    fn expand(&mut self, stats: &ZramPoolStats) -> Result<()> {
        let disksize = self.calculate_next_disksize(stats);

        info!(
            "ZramPool: expanding — adding device (disksize={}MB, pool_util={}%, ratio={:.2}x, phys={}%)",
            disksize / (1024 * 1024),
            stats.utilization_percent,
            stats.compression_ratio,
            stats.phys_usage_percent
        );

        self.create_device(disksize)?;
        self.last_expansion = Some(Instant::now());
        self.save_device_info()?;

        Ok(())
    }

    /// Check if pool should contract (remove last device)
    fn should_contract(&self, stats: &ZramPoolStats) -> bool {
        // 1. Keep at least INITIAL_DEVICES (4) devices running at all times
        if self.active_count() <= 4 {
            return false;
        }

        // 2. Pool underutilized
        if stats.utilization_percent > self.config.contract_threshold {
            return false;
        }

        // 3. Last device nearly empty
        if let Some(last_dev) = self.devices.last() {
            if last_dev.state != ZramDeviceState::Active {
                return false;
            }
            if let Some(dev_stats) = get_device_stats(&last_dev.sysfs_path, last_dev.disksize) {
                let dev_util = dev_stats.memory_utilization();
                if dev_util > 5 {
                    return false;
                }
            }
        }

        // 4. Low utilization sustained
        if let Some(since) = self.low_util_since {
            if since.elapsed().as_secs() < self.config.contract_stability {
                return false;
            }
        } else {
            return false;
        }

        // 5. Cooldown since last contraction
        if let Some(last) = self.last_contraction {
            if last.elapsed().as_secs() < 60 {
                return false;
            }
        }

        true
    }

    /// Single non-blocking swapoff attempt for a device at the given index.
    /// On success, finalizes hot-remove and returns true.
    /// On failure, increments drain_attempts and returns false.
    fn try_drain_device(&mut self, idx: usize) -> Result<bool> {
        let dev_path = self.devices[idx].dev_path.clone();
        let dev_id = self.devices[idx].id;

        let succeeded = Command::new("swapoff")
            .arg(&dev_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !succeeded {
            self.devices[idx].drain_attempts += 1;
            return Ok(false);
        }

        let sysfs_path = self.devices[idx].sysfs_path.clone();
        let unit_name = self.devices[idx].unit_name.clone();

        let _ = systemctl(SystemctlAction::Stop, &unit_name);
        let _ = std::fs::write(format!("{}/reset", sysfs_path), "1");
        if Path::new(ZRAM_HOT_REMOVE).exists() {
            let _ = std::fs::write(ZRAM_HOT_REMOVE, dev_id.to_string());
        }
        let unit_path = format!("/run/systemd/system/{}", unit_name);
        let _ = std::fs::remove_file(unit_path);
        let _ = systemctl(SystemctlAction::DaemonReload, "");

        self.devices.remove(idx);
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

        // Also save pool metadata
        let meta = format!(
            "devices={}\nmax_devices={}",
            self.active_count(),
            self.config.max_devices
        );
        std::fs::write(format!("{}/zram/pool_meta", WORK_DIR), &meta)?;

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

        let check_interval = self.config.check_interval;
        let mut log_counter: u64 = 0;

        loop {
            thread::sleep(Duration::from_secs(check_interval));

            if crate::is_shutdown() {
                break;
            }

            let stats = match self.get_pool_stats() {
                Some(s) => s,
                None => continue,
            };

            // Periodic log (every ~30s)
            log_counter += 1;
            if log_counter * check_interval >= 30 {
                log_counter = 0;
                info!(
                    "ZramPool: {} dev(s), util={}%, ratio={:.2}x, phys={}% ({}MB/{}MB)",
                    stats.device_count,
                    stats.utilization_percent,
                    stats.compression_ratio,
                    stats.phys_usage_percent,
                    stats.total_phys_used / (1024 * 1024),
                    self.ram_total / (1024 * 1024)
                );
            }

            // Track low utilization for contraction stability
            if stats.utilization_percent <= self.config.contract_threshold {
                if self.low_util_since.is_none() {
                    self.low_util_since = Some(Instant::now());
                }
            } else {
                self.low_util_since = None;
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
    pub compr_data_size: u64,
    pub mem_used_total: u64,
    pub mem_limit: u64,
    pub disksize: u64,
    pub same_pages: u64,
    pub pages_compacted: u64,
}

impl ZramStats {
    pub fn compression_ratio(&self) -> f64 {
        if self.compr_data_size == 0 {
            0.0
        } else {
            self.orig_data_size as f64 / self.compr_data_size as f64
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
    let mut total_same: u64 = 0;
    let mut total_compacted: u64 = 0;
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
            mem_limit = stats.mem_limit; // Use last device's limit
            total_same += stats.same_pages;
            total_compacted += stats.pages_compacted;
            found = true;
        }
    }

    if !found {
        return None;
    }

    Some(ZramStats {
        orig_data_size: total_orig,
        compr_data_size: total_compr,
        mem_used_total: total_phys,
        mem_limit,
        disksize: total_disksize,
        same_pages: total_same,
        pages_compacted: total_compacted,
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
        compr_data_size: fields[1],
        mem_used_total: fields[2],
        mem_limit: fields[3],
        disksize,
        same_pages: fields.get(5).copied().unwrap_or(0),
        pages_compacted: fields.get(6).copied().unwrap_or(0),
    })
}
