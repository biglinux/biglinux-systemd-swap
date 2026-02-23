// SwapFC - Dynamic swap file management
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::config::{Config, WORK_DIR};
use crate::defaults;
use crate::helpers::{force_remove, get_fstype, makedirs, parse_size as parse_size_shared, run_cmd_output};
use crate::meminfo::{get_free_ram_percent, get_free_swap_percent_effective};
use crate::systemd::{
    gen_swap_unit, notify_ready, notify_status, swapoff, systemctl, SystemctlAction,
};
use crate::{debug, info, is_shutdown, warn};

#[derive(Error, Debug)]
pub enum SwapFileError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Helper error: {0}")]
    Helper(#[from] crate::helpers::HelperError),
    #[error("Systemd error: {0}")]
    Systemd(#[from] crate::systemd::SystemdError),
    #[error("Invalid swapfile_path")]
    InvalidPath,
    #[error("Unsupported filesystem (requires btrfs, ext4, or xfs)")]
    UnsupportedFs,
    #[error("Not enough space")]
    NoSpace,
}

pub type Result<T> = std::result::Result<T, SwapFileError>;

/// Information about an individual swap file from /proc/swaps
#[derive(Debug, Clone)]
pub struct SwapFileInfo {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub priority: i32,
}

impl SwapFileInfo {
    /// Calculate usage percentage for this specific file
    pub fn usage_percent(&self) -> u8 {
        if self.size_bytes == 0 {
            return 0;
        }
        ((self.used_bytes * 100) / self.size_bytes) as u8
    }

    /// Check if file is nearly empty (candidate for removal)
    pub fn is_nearly_empty(&self, threshold: u8) -> bool {
        self.usage_percent() <= threshold
    }
}

/// SwapFC configuration
#[derive(Debug)]
pub struct SwapFileConfig {
    pub path: PathBuf,
    /// Base chunk size (initial allocation size)
    pub chunk_size: u64,
    pub max_count: u32,
    pub min_count: u32,
    pub free_ram_perc: u8,
    pub free_swap_perc: u8,
    pub remove_free_swap_perc: u8,
    pub frequency: u64,
    /// Priority for swap files (-1 = auto-calculate based on storage type)
    /// Individual file usage threshold for removal consideration (default: 30%)
    pub shrink_threshold: u8,
    /// Safe headroom percentage to maintain in other files after migration (default: 40%)
    pub safe_headroom: u8,
    /// Use sparse backing + loop device for swap files.
    ///
    /// When `true`:
    ///   - A loop device (`--direct-io=on`) is always created; `swapon` targets
    ///     the loop device, enabling I/O scheduler and queue tuning.
    ///   - direct-io=on bypasses page cache, preventing memory deadlock
    ///     during swap writeback under pressure.
    ///   - Sparse file (truncate). Blocks are allocated on-demand by btrfs.
    ///   - Compression is handled by zswap (in RAM), not the filesystem.
    ///
    /// When `false` (default): fallocate + nodatacow + direct swapon (no loop).
    pub sparse_loop_backing: bool,
    /// Size in bytes for each swap file created during the growth phase
    /// (sparse loop only). Typically 2× the initial chunk_size.
    /// 0 = not configured (falls back to chunk_size).
    pub growth_chunk_size: u64,
    /// NOCOW (chattr +C) on btrfs swap files.
    /// Default: true (prevents btrfs deadlock under memory pressure).
    pub nocow: bool,
}



/// Reject paths that point at critical system directories or are not absolute.
///
/// Accepts paths under `/`, `/var`, `/home`, `/swap`, `/mnt`, `/media`, `/tmp`,
/// `/run/user` and similar writable locations. Rejects bare system directories
/// such as `/etc`, `/sys`, `/proc`, `/dev`, `/bin`, `/sbin`, `/usr`, `/lib`,
/// `/boot`, and `/run` itself.
fn validate_swapfile_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    // Exact directories that must never be used as a swap directory
    const FORBIDDEN: &[&str] = &[
        "/etc",
        "/sys",
        "/proc",
        "/dev",
        "/run",
        "/bin",
        "/sbin",
        "/usr",
        "/lib",
        "/lib64",
        "/boot",
        "/snap",
        "/lost+found",
    ];
    let path_str = path.to_string_lossy();
    for forbidden in FORBIDDEN {
        if path_str == *forbidden || path_str.starts_with(&format!("{}/", forbidden)) {
            return false;
        }
    }
    true
}

impl SwapFileConfig {
    /// Create config from parsed Config file
    pub fn from_config(config: &Config) -> Result<Self> {
        let path = config.get("swapfile_path").unwrap_or(defaults::SWAPFILE_PATH).to_string();
        let path = PathBuf::from(path.trim_end_matches('/'));
        if !validate_swapfile_path(&path) {
            return Err(SwapFileError::InvalidPath);
        }

        let chunk_size_str = config.get("swapfile_chunk_size").unwrap_or(defaults::SWAPFILE_CHUNK_SIZE).to_string();
        let chunk_size = parse_size_shared(&chunk_size_str).map_err(|_| SwapFileError::InvalidPath)?;
        let sparse = config.get_bool("swapfile_sparse_loop");
        let chunk_size = chunk_size.max(if sparse {
            128 * 1024 * 1024
        } else {
            512 * 1024 * 1024
        });

        let max_count: u32 = config.get_as("swapfile_max_count").unwrap_or(defaults::SWAPFILE_MAX_COUNT);
        let max_count = max_count.clamp(1, 28);

        let min_count: u32 = config.get_as("swapfile_min_count").unwrap_or(defaults::SWAPFILE_MIN_COUNT);
        let frequency: u64 = config.get_as::<u32>("swapfile_frequency").unwrap_or(defaults::SWAPFILE_FREQUENCY) as u64;
        let frequency = frequency.clamp(1, 86400);

        let shrink_threshold: u8 =
            config.get_as::<u32>("swapfile_shrink_threshold").unwrap_or(defaults::SWAPFILE_SHRINK_THRESHOLD as u32) as u8;
        let shrink_threshold = shrink_threshold.clamp(10, 50);

        let safe_headroom: u8 =
            config.get_as::<u32>("swapfile_safe_headroom").unwrap_or(defaults::SWAPFILE_SAFE_HEADROOM as u32) as u8;
        let safe_headroom = safe_headroom.clamp(20, 60);

        Ok(Self {
            path,
            chunk_size,
            max_count,
            min_count,
            free_ram_perc: config.get_as::<u32>("swapfile_free_ram_perc").unwrap_or(defaults::SWAPFILE_FREE_RAM_PERC as u32) as u8,
            free_swap_perc: config.get_as::<u32>("swapfile_free_swap_perc").unwrap_or(defaults::SWAPFILE_FREE_SWAP_PERC as u32) as u8,
            remove_free_swap_perc: config.get_as::<u32>("swapfile_remove_free_swap_perc").unwrap_or(defaults::SWAPFILE_REMOVE_FREE_SWAP_PERC as u32) as u8,
            frequency,
            shrink_threshold,
            safe_headroom,
            sparse_loop_backing: sparse,
            growth_chunk_size: {
                let s = config.get("swapfile_growth_chunk_size").unwrap_or("").to_string();
                if s.is_empty() {
                    0
                } else {
                    parse_size_shared(&s).unwrap_or(0)
                }
            },
            nocow: {
                let s = config.get("swapfile_nocow").unwrap_or(defaults::SWAPFILE_NOCOW).to_string();
                !matches!(s.as_str(), "0" | "false" | "no" | "off")
            },
        })
    }
}

/// Optimize a loop block device's I/O queue parameters for swap.
///
/// Scheduler is always "none" — loop devices sit atop a real block device
/// that already has its own scheduler. Adding another causes deadlock
/// under extreme memory pressure (proven by testing).
fn tune_loop_device(loop_dev: &str) {
    let dev_name = loop_dev.trim_start_matches("/dev/");
    let queue_path = format!("/sys/block/{}/queue", dev_name);

    if !Path::new(&queue_path).is_dir() {
        warn!("swapFC: cannot tune {} - sysfs queue not found", dev_name);
        return;
    }

    let _ = fs::write(format!("{}/rotational", queue_path), "0");
    let _ = fs::write(format!("{}/iostats", queue_path), "0");
    let _ = fs::write(format!("{}/add_random", queue_path), "0");

    // Set scheduler to "none" (passthrough)
    let scheduler_path = format!("{}/scheduler", queue_path);
    if fs::write(&scheduler_path, "none").is_ok() {
        info!("swapFC: {} scheduler set to [none]", dev_name);
    } else {
        warn!("swapFC: failed to set scheduler none on {}", dev_name);
    }

    // Queue parameters
    let _ = fs::write(format!("{}/nomerges", queue_path), "0");
    let wbt_path = format!("{}/wbt_lat_usec", queue_path);
    if Path::new(&wbt_path).exists() {
        let _ = fs::write(&wbt_path, "75000");
    }
    let _ = fs::write(format!("{}/max_sectors_kb", queue_path), "512");
    let _ = fs::write(format!("{}/rq_affinity", queue_path), "1");
}

/// Re-apply volatile queue parameters that swapon may reset.
/// Called AFTER the swap unit is started.
/// Only sets the two critical params; everything else stays at kernel defaults.
fn retune_loop_queue(loop_dev: &str) {
    let dev_name = loop_dev.trim_start_matches("/dev/");
    let queue_path = format!("/sys/block/{}/queue", dev_name);
    if !Path::new(&queue_path).is_dir() {
        info!("swapFC: retune {} - queue path not found", dev_name);
        return;
    }
    let _ = fs::write(format!("{}/nomerges", queue_path), "0");
    let wbt_path = format!("{}/wbt_lat_usec", queue_path);
    if Path::new(&wbt_path).exists() {
        let _ = fs::write(&wbt_path, "75000");
    }
    let _ = fs::write(format!("{}/max_sectors_kb", queue_path), "512");
    let _ = fs::write(format!("{}/rq_affinity", queue_path), "1");
}

/// SwapFC manager - supports btrfs, ext4, and xfs
pub struct SwapFile {
    config: SwapFileConfig,
    allocated: u32,
    /// True if path is on btrfs (for subvolume/nodatacow handling)
    is_btrfs: bool,
    /// Track the size of each allocated file (for proper cleanup and stats)
    file_sizes: Vec<u64>,
    /// Cooldown: last time a swap file was created (prevents runaway creation)
    last_creation: Option<Instant>,
    /// Escalating cooldown in seconds (doubles on each creation, resets when swap is consumed)
    cooldown_secs: u64,
    /// Previous free_swap percentage (to detect when swap is actually being consumed)
    prev_free_swap: u8,
    /// Whether ZSWAP is active (kernel-level compression with writeback)
    is_zswap_active: bool,
    /// Disk full flag: stops expansion attempts until space is freed
    disk_full: bool,
}

impl SwapFile {
    /// Create new SwapFC manager
    pub fn new(config: &Config) -> Result<Self> {
        let swapfile_config = SwapFileConfig::from_config(config)?;

        info!(
            "swapFC: chunk={}MB, sparse_loop={}",
            swapfile_config.chunk_size / (1024 * 1024),
            swapfile_config.sparse_loop_backing,
        );

        notify_status("Monitoring memory status...");

        // Create parent directories
        makedirs(swapfile_config.path.parent().unwrap_or(Path::new("/")))?;

        // Detect filesystem type
        let fstype = get_fstype(&swapfile_config.path);
        let is_btrfs = fstype.as_deref() == Some("btrfs");

        // Verify supported filesystem
        match fstype.as_deref() {
            Some("btrfs") | Some("ext4") | Some("xfs") => {}
            Some(fs) => {
                warn!(
                    "swapFC: unsupported filesystem '{}', swap files may not work correctly",
                    fs
                );
            }
            None => {
                warn!("swapFC: could not detect filesystem type");
            }
        }

        // Setup swap directory based on filesystem type
        if is_btrfs {
            // For btrfs: create subvolume with nodatacow for swap
            let is_subvolume = is_btrfs_subvolume(&swapfile_config.path);

            if !is_subvolume {
                if swapfile_config.path.exists() {
                    warn!("swapFC: path exists but not a subvolume, removing...");
                    if swapfile_config.path.is_dir() {
                        fs::remove_dir_all(&swapfile_config.path)?;
                    } else {
                        fs::remove_file(&swapfile_config.path)?;
                    }
                }

                // Try to create btrfs subvolume
                let output = Command::new("btrfs")
                    .args(["subvolume", "create"])
                    .arg(&swapfile_config.path)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!("swapFC: btrfs subvolume create failed: {}", stderr.trim());

                    // Fallback: try creating as regular directory
                    info!("swapFC: falling back to regular directory");
                    fs::create_dir_all(&swapfile_config.path)?;

                    // Set nodatacow attribute if configured
                    if swapfile_config.nocow {
                        let _ = Command::new("chattr")
                            .args(["+C"])
                            .arg(&swapfile_config.path)
                            .status();
                    }

                    info!(
                        "swapFC: created directory (non-subvolume) at {:?}",
                        swapfile_config.path
                    );
                } else {
                    // Set nodatacow on subvolume for safe swap I/O under memory pressure.
                    // Without NOCOW, btrfs block allocation during swap writes can deadlock.
                    if swapfile_config.nocow {
                        let _ = Command::new("chattr")
                            .args(["+C"])
                            .arg(&swapfile_config.path)
                            .status();
                    }

                    info!(
                        "swapFC: created btrfs subvolume at {:?}",
                        swapfile_config.path
                    );
                }
            } else {
                // Subvolume already exists — ensure nocow attribute matches config.
                // A previous run may have set +C that we need to clear (or vice-versa).
                if swapfile_config.nocow {
                    let _ = Command::new("chattr")
                        .args(["+C"])
                        .arg(&swapfile_config.path)
                        .status();
                } else {
                    let _ = Command::new("chattr")
                        .args(["-C"])
                        .arg(&swapfile_config.path)
                        .status();
                }
            }
        } else {
            // For ext4/xfs: just create directory
            if !swapfile_config.path.exists() {
                fs::create_dir_all(&swapfile_config.path)?;
                info!(
                    "swapFC: created swap directory at {:?}",
                    swapfile_config.path
                );
            }
        }

        // Check btrfs mount options for loop-backed swap files.
        // autodefrag MUST be disabled: it causes extra I/O on swap file extents
        // and can deadlock under memory pressure when using loop devices.
        // noatime MUST be enabled: avoids unnecessary metadata writes.
        // compress-force=zstd:1: fastest zstd level for latency-sensitive swap I/O.
        if is_btrfs {
            if let Ok(output) = Command::new("findmnt")
                .args(["-n", "-o", "OPTIONS", "--target"])
                .arg(&swapfile_config.path)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
            {
                let opts = String::from_utf8_lossy(&output.stdout);
                let needs_no_autodefrag = opts.contains("autodefrag");
                let needs_noatime = !opts.contains("noatime");
                // Downgrade zstd level for swap — zstd:1 is ~3x faster than zstd:3
                // with only ~5% less ratio. Critical under memory pressure when
                // btrfs compresses swap-back pages written by zswap shrinker.
                let needs_zstd1 = !swapfile_config.nocow
                    && (opts.contains("zstd:2")
                        || opts.contains("zstd:3")
                        || opts.contains("zstd:4")
                        || opts.contains("zstd:5"));

                if needs_no_autodefrag || needs_noatime || needs_zstd1 {
                    let mut remount_opts = String::from("remount");
                    if needs_no_autodefrag {
                        remount_opts.push_str(",noautodefrag");
                        info!(
                            "swapFC: disabling autodefrag on {:?} for loop swap stability",
                            swapfile_config.path
                        );
                    }
                    if needs_noatime {
                        remount_opts.push_str(",noatime");
                        info!(
                            "swapFC: enabling noatime on {:?} to reduce metadata I/O",
                            swapfile_config.path
                        );
                    }
                    if needs_zstd1 {
                        remount_opts.push_str(",compress-force=zstd:1");
                        info!(
                            "swapFC: downgrading compression to zstd:1 on {:?} for swap latency",
                            swapfile_config.path
                        );
                    }
                    let status = Command::new("mount")
                        .args(["-o", &remount_opts])
                        .arg(&swapfile_config.path)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    if status.map(|s| !s.success()).unwrap_or(true) {
                        warn!(
                            "swapFC: failed to remount {:?} with {}. \
                             Update mount options in /etc/fstab manually.",
                            swapfile_config.path, remount_opts
                        );
                    }
                }
            }
        }

        makedirs(format!("{}/swapfile", WORK_DIR))?;

        // Check if ZSWAP is active
        let is_zswap_active = crate::zswap::is_enabled();
        if is_zswap_active {
            info!("swapFC: ZSWAP detected active - swapfiles serve as writeback backing");
        }

        Ok(Self {
            config: swapfile_config,
            allocated: 0,
            is_btrfs,
            file_sizes: Vec::new(),
            last_creation: None,
            cooldown_secs: if is_zswap_active { 5 } else { 15 },
            prev_free_swap: 100,
            is_zswap_active,
            disk_full: false,
        })
    }

    /// Enable zswap mode: set is_zswap_active and adjust cooldown.
    /// Call this BEFORE create_initial_swap() when SwapMode is ZswapSwapfc.
    pub fn enable_zswap_mode(&mut self) {
        if !self.is_zswap_active {
            self.is_zswap_active = true;
            self.cooldown_secs = 5;
            info!(
                "swapFC: ZSWAP mode enabled - initial_count={} chunk={}MB growth={}MB",
                self.config.min_count,
                self.config.chunk_size / (1024 * 1024),
                if self.config.growth_chunk_size > 0 {
                    self.config.growth_chunk_size / (1024 * 1024)
                } else {
                    self.config.chunk_size * 2 / (1024 * 1024)
                },
            );
        }
    }

    /// Read information about all swap files from /proc/swaps
    fn get_swapfiles_info(&self) -> Vec<SwapFileInfo> {
        let mut files = Vec::new();

        let content = match std::fs::read_to_string("/proc/swaps") {
            Ok(c) => c,
            Err(_) => return files,
        };

        // Skip header: Filename Type Size Used Priority
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 5 {
                continue;
            }

            let path = PathBuf::from(fields[0]);

            // Filter only our swap files (in the configured directory or loop devices)
            // Note: use string comparison for /dev/loop* — Path::starts_with does component
            // matching, so "/dev/loop10".starts_with("/dev/loop") is false ("loop10" ≠ "loop").
            let path_str = path.to_string_lossy();
            let is_our_file = path.starts_with(&self.config.path)
                || (path_str.starts_with("/dev/loop") && self.is_our_loop_device(&path));

            if !is_our_file {
                continue;
            }

            let size_kb: u64 = fields[2].parse().unwrap_or(0);
            let used_kb: u64 = fields[3].parse().unwrap_or(0);
            let priority: i32 = fields[4].parse().unwrap_or(0);

            files.push(SwapFileInfo {
                path,
                size_bytes: size_kb * 1024,
                used_bytes: used_kb * 1024,
                priority,
            });
        }

        // Sort by priority (higher priority first - used first by kernel)
        files.sort_by(|a, b| b.priority.cmp(&a.priority));
        files
    }

    /// Check if a loop device belongs to us
    fn is_our_loop_device(&self, loop_path: &Path) -> bool {
        // Scan all loop_info files in WORK_DIR, not just up to self.allocated.
        // During adoption (adopt_existing_swapfiles), self.allocated is still 0,
        // so a 1..=self.allocated range would never iterate.
        let loop_dir = format!("{}/swapfile", WORK_DIR);
        let Ok(entries) = std::fs::read_dir(&loop_dir) else {
            return false;
        };
        let loop_dev_str = loop_path.to_string_lossy();
        for entry in entries.flatten() {
            let fname = entry.file_name();
            if !fname.to_string_lossy().starts_with("loop_") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(entry.path()) {
                if content.lines().next().map(str::trim) == Some(loop_dev_str.as_ref()) {
                    return true;
                }
            }
        }
        false
    }

    /// Find a safe candidate for removal
    /// Returns None if no removal is safe
    fn find_safe_removal_candidate<'a>(
        &self,
        files: &'a [SwapFileInfo],
    ) -> Option<&'a SwapFileInfo> {
        if files.len() <= self.config.min_count as usize {
            return None; // Don't remove below minimum
        }

        // Find files with low usage (< shrink_threshold%)
        let mut candidates: Vec<&SwapFileInfo> = files
            .iter()
            .filter(|f| f.is_nearly_empty(self.config.shrink_threshold))
            .collect();

        if candidates.is_empty() {
            return None; // No file is empty enough
        }

        // Sort candidates by priority ASCENDING (Lowest first)
        // We want to remove low-priority files (created last, usually larger) first
        // to scale down properly instead of leaving a giant tail file alone.
        candidates.sort_by(|a, b| a.priority.cmp(&b.priority));

        // For each candidate, verify if it's SAFE to remove
        candidates
            .into_iter()
            .find(|&candidate| self.can_safely_remove(candidate, files))
            .map(|v| v as _)
    }

    /// Verify if it's safe to remove a specific file
    /// Safe if: data from the file can be absorbed by others with headroom
    fn can_safely_remove(&self, target: &SwapFileInfo, all_files: &[SwapFileInfo]) -> bool {
        // Calculate free space in OTHER files
        let mut other_total_size: u64 = 0;
        let mut other_total_used: u64 = 0;

        for file in all_files {
            if file.path != target.path {
                other_total_size += file.size_bytes;
                other_total_used += file.used_bytes;
            }
        }

        // If no other files, not safe to remove
        if other_total_size == 0 {
            return false;
        }

        // Free space in other files
        let other_free_space = other_total_size.saturating_sub(other_total_used);

        // Data that needs to be migrated from target file
        let data_to_migrate = target.used_bytes;

        // Verify if there's enough space WITH safety margin
        // Want to maintain at least safe_headroom% free after migration
        let required_headroom = (other_total_size * self.config.safe_headroom as u64) / 100;
        let required_free = data_to_migrate + required_headroom;

        if other_free_space < required_free {
            debug!(
                "swapFC: removing {} not safe - needs {}MB free, has {}MB",
                target.path.display(),
                required_free / (1024 * 1024),
                other_free_space / (1024 * 1024)
            );
            return false;
        }

        true
    }

    /// Remove a specific swap file by path
    fn destroy_swapfile_by_path(&mut self, path: &Path) -> Result<()> {
        // Find which index this file corresponds to
        let file_index = self.find_file_index(path);

        notify_status(&format!("Deallocating swap file {}...", path.display()));

        // First: swapoff (kernel will migrate data to other files)
        if let Err(e) = swapoff(&path.to_string_lossy()) {
            warn!("swapFC: swapoff failed for {}: {}", path.display(), e);
            return Err(SwapFileError::Io(std::io::Error::other("swapoff failed")));
        }

        // If it's a loop device, get the backing file
        // Use string comparison: Path::starts_with does component matching.
        let is_loop = path.to_string_lossy().starts_with("/dev/loop");
        let backing_file = if is_loop {
            self.get_backing_file_for_loop(path)
        } else {
            Some(path.to_path_buf())
        };

        if is_loop {
            // Detach loop device
            let _ = Command::new("losetup")
                .args(["-d", &path.to_string_lossy()])
                .status();
        }

        // Remove backing file
        if let Some(ref backing) = backing_file {
            force_remove(backing, false);
        }

        // Clean up systemd unit
        if let Some(idx) = file_index {
            let tag = format!("swapfile_{}", idx);
            for unit_path in crate::helpers::find_swap_units() {
                if let Ok(content) = crate::helpers::read_file(&unit_path) {
                    if content.contains(&tag) {
                        force_remove(&unit_path, true);
                        break;
                    }
                }
            }

            // Clean up loop info file
            let loop_info_path = format!("{}/swapfile/loop_{}", WORK_DIR, idx);
            force_remove(&loop_info_path, false);

            // Update file_sizes if we tracked this file
            if idx <= self.file_sizes.len() as u32 {
                self.file_sizes.remove((idx - 1) as usize);
            }
        }

        self.allocated = self.allocated.saturating_sub(1);

        info!("swapFC: {} removed successfully", path.display());
        notify_status("Monitoring memory status...");
        Ok(())
    }

    /// Find the index of a file/loop device in our managed files
    fn find_file_index(&self, path: &Path) -> Option<u32> {
        // Check if it's a direct file in our directory
        if path.starts_with(&self.config.path) {
            if let Some(name) = path.file_name() {
                return name.to_string_lossy().parse().ok();
            }
        }

        // Check loop device info files
        for i in 1..=self.allocated {
            let loop_info_path = format!("{}/swapfile/loop_{}", WORK_DIR, i);
            if let Ok(content) = fs::read_to_string(&loop_info_path) {
                let lines: Vec<&str> = content.lines().collect();
                if !lines.is_empty() && lines[0] == path.to_string_lossy() {
                    return Some(i);
                }
            }
        }

        None
    }

    /// Get the backing file for a loop device
    fn get_backing_file_for_loop(&self, loop_path: &Path) -> Option<PathBuf> {
        // Scan all loop_info files (not bounded by self.allocated; may be called
        // during adoption before allocated is set).
        let loop_dir = format!("{}/swapfile", WORK_DIR);
        let Ok(entries) = std::fs::read_dir(&loop_dir) else {
            return None;
        };
        let loop_dev_str = loop_path.to_string_lossy();
        for entry in entries.flatten() {
            let fname = entry.file_name();
            if !fname.to_string_lossy().starts_with("loop_") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(entry.path()) {
                let mut lines = content.lines();
                let Some(dev) = lines.next() else { continue };
                let Some(backing) = lines.next() else {
                    continue;
                };
                if dev.trim() == loop_dev_str.as_ref() {
                    return Some(PathBuf::from(backing.trim()));
                }
            }
        }
        None
    }

    /// Adopt swap files that already exist from a previous run.
    /// Called before create_initial_swap() so we never swapoff active files on restart.
    fn adopt_existing_swapfiles(&mut self) {
        // For sparse loop-backed mode, reconstruct loop info files from losetup
        // before calling get_swapfiles_info(), which requires those files to exist.
        // This handles the restart case where WORK_DIR was wiped but loop devices
        // are still active and backed by our sparse files.
        if self.config.sparse_loop_backing {
            self.reconstruct_loop_info_from_losetup();
        }

        let existing = self.get_swapfiles_info();
        if existing.is_empty() {
            return;
        }

        let mut max_num: u32 = 0;

        for info in &existing {
            if let Some(name) = info.path.file_name() {
                if let Ok(n) = name.to_string_lossy().parse::<u32>() {
                    max_num = max_num.max(n);
                }
            }
            // For loop devices, derive the backing file number from the loop info file.
            if info.path.to_string_lossy().starts_with("/dev/loop") {
                let loop_name = info.path.to_string_lossy();
                // Find the matching loop info file we just wrote
                for i in 1..=28u32 {
                    let loop_info = format!("{}/swapfile/loop_{}", WORK_DIR, i);
                    if let Ok(content) = fs::read_to_string(&loop_info) {
                        if content.lines().next() == Some(&loop_name) {
                            max_num = max_num.max(i);
                            break;
                        }
                    }
                }
            }
        }

        if max_num > 0 {
            info!(
                "swapFC: adopting {} existing file(s) (max index: {})",
                existing.len(),
                max_num
            );
            self.allocated = max_num;

            // Reconstruct file_sizes from disk metadata
            self.file_sizes.clear();
            for i in 1..=max_num {
                let path = self.config.path.join(i.to_string());
                let size = path
                    .metadata()
                    .map(|m| m.len())
                    .unwrap_or(self.config.chunk_size);
                self.file_sizes.push(size);
            }
        }
    }

    /// Rebuild per-index loop info files from `losetup -l` output.
    ///
    /// Called during adoption at startup when WORK_DIR was cleared (e.g. after
    /// a restart).  Maps each active loop device whose backing file lives in
    /// `self.config.path` back to its numeric index (the file's own name),
    /// then writes `{WORK_DIR}/swapfile/loop_N` so that `is_our_loop_device()`
    /// and `get_swapfiles_info()` can recognise them normally.
    fn reconstruct_loop_info_from_losetup(&self) {
        // losetup -l --noheadings -o NAME,BACK-FILE
        let output = match Command::new("losetup")
            .args(["-l", "--noheadings", "-o", "NAME,BACK-FILE"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return,
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let loop_dev = parts[0];
            let backing = parts[1];

            // Skip loop devices whose backing file has been deleted.
            // losetup appends "(deleted)" when the inode is unlinked but
            // the loop device keeps its file descriptor open — these are
            // from previous sessions whose files were already removed.
            // Detach them to prevent loop device accumulation.
            if parts.get(2).copied() == Some("(deleted)") {
                info!(
                    "swapFC: detaching loop {} with deleted backing file",
                    loop_dev
                );
                let _ = Command::new("losetup").args(["-d", loop_dev]).status();
                continue;
            }

            let backing_path = PathBuf::from(backing);

            // Extract the numeric index from the backing file name.
            // NOTE: btrfs subvolumes cause losetup to report the backing file path
            // relative to the subvolume root (e.g. "/1" instead of "/swapfile/1").
            // We cannot rely on the reported path prefix; match by numeric name only.
            let idx: u32 = match backing_path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.parse().ok())
            {
                Some(n) => n,
                None => continue,
            };

            // Verify that this numeric file exists in our managed directory.
            let canonical_backing = self.config.path.join(idx.to_string());
            let actual_backing = if canonical_backing.exists() {
                canonical_backing
            } else {
                continue;
            };

            let loop_info_path = format!("{}/swapfile/loop_{}", WORK_DIR, idx);
            let _ = fs::write(
                &loop_info_path,
                format!("{}\n{}", loop_dev, actual_backing.display()),
            );
            info!(
                "swapFC: reconstructed loop info: {} → {} (index {})",
                loop_dev,
                actual_backing.display(),
                idx
            );
        }
    }

    /// Create initial swap files (needed for zswap backing / zram overflow)
    pub fn create_initial_swap(&mut self) -> Result<()> {
        // Adopt any files left from a previous run before creating new ones.
        // This prevents swapping off active files under memory pressure on restart.
        self.adopt_existing_swapfiles();

        // After adoption, eagerly shed empty surplus files without waiting for the
        // 60-second contraction cooldown. Prevents accumulating ghost swapfiles from
        // previous sessions (e.g. benchmarks) that left multiple empty files active.
        if self.allocated > self.config.min_count {
            self.shed_excess_empty_adopted();
        }

        // Remove physical files in our directory that are NOT in /proc/swaps.
        // These are stale from crashes or force-reboots and waste disk space.
        self.cleanup_stale_disk_files();

        while self.allocated < self.config.min_count {
            if let Err(e) = self.create_swapfile() {
                warn!(
                    "swapFC: initial swap creation stopped at {}/{}: {}",
                    self.allocated, self.config.min_count, e
                );
                break;
            }
        }
        if self.allocated == 0 {
            return Err(SwapFileError::NoSpace);
        }

        Ok(())
    }

    /// Re-apply volatile queue parameters on all active loop devices.
    /// Called after initial creation and after udevadm settle.
    fn retune_all_loops(&self) {
        let loop_dir = format!("{}/swapfile", WORK_DIR);
        let entries = match fs::read_dir(&loop_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("loop_") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(entry.path()) {
                let loop_dev = content.lines().next().unwrap_or("").trim();
                if loop_dev.starts_with("/dev/loop") {
                    retune_loop_queue(loop_dev);
                }
            }
        }
    }

    /// Enforce read_ahead_kb on all active loop devices.
    /// The kernel loop driver overrides read_ahead_kb after swapon and udev events,
    /// so we use blockdev --setra (ioctl-based) and re-apply periodically.
    fn enforce_loop_readahead(&self) {
        let ra_sectors = 16; // 8KB = 16 sectors
        let loop_dir = format!("{}/swapfile", WORK_DIR);
        let Ok(entries) = fs::read_dir(&loop_dir) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("loop_") {
                continue;
            }
            let Ok(content) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let loop_dev = content.lines().next().unwrap_or("").trim().to_string();
            if loop_dev.starts_with("/dev/loop") {
                let _ = Command::new("blockdev")
                    .args(["--setra", &ra_sectors.to_string(), &loop_dev])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }

    /// Remove empty adopted swapfiles above min_count at startup (no cooldown).
    /// Iterates lowest-priority (last created) first for cleanest teardown order.
    fn shed_excess_empty_adopted(&mut self) {
        let swap_files = self.get_swapfiles_info();

        // Collect paths to remove: empty files, lowest priority first
        let to_remove: Vec<PathBuf> = swap_files
            .iter()
            .rev() // swap_files sorted high→low priority; reverse = low→high = last-created first
            .filter(|f| f.used_bytes == 0)
            .map(|f| f.path.clone())
            .collect();

        for path in to_remove {
            if self.allocated <= self.config.min_count {
                break;
            }
            info!(
                "swapFC: startup cleanup: removing empty surplus file {} ({} active, min {})",
                path.display(),
                self.allocated,
                self.config.min_count
            );
            let _ = self.destroy_swapfile_by_path(&path);
        }
    }

    /// Remove physical files in our directory that are NOT in /proc/swaps.
    /// These are leftovers from force-reboots, crashes, or old benchmark sessions.
    fn cleanup_stale_disk_files(&self) {
        let active_swaps = self.get_swapfiles_info();

        // Build set of "active" disk paths:
        // - Direct swap files (non-loop) → their path is the disk file
        // - Loop-backed files → the disk file is the BACKING file, not /dev/loopN
        let mut active: std::collections::HashSet<PathBuf> =
            active_swaps.iter().map(|f| f.path.clone()).collect();

        if self.config.sparse_loop_backing {
            // Add backing file paths for any active loop devices
            for info in &active_swaps {
                if info.path.to_string_lossy().starts_with("/dev/loop") {
                    if let Some(backing) = self.get_backing_file_for_loop(&info.path) {
                        active.insert(backing);
                    }
                }
            }
            // Detach loop devices whose backing file is NOT active (orphaned by
            // a previous stop timeout or forced shutdown). This prevents loop device
            // leaks accumulating across restarts.
            self.detach_orphaned_loops(&active);
        }

        let Ok(entries) = std::fs::read_dir(&self.config.path) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            // Only touch numeric-named files (our swapfiles)
            let is_ours = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.parse::<u32>().is_ok())
                .unwrap_or(false);
            if is_ours && !active.contains(&path) {
                info!("swapFC: removing stale disk file {}", path.display());
                force_remove(&path, false);
            }
        }
    }

    /// Detach any loop device whose backing file is not in `active_backings`.
    /// These are loops left attached without active swap — e.g. after a stop
    /// timeout where only some loops were swapped off before the process was killed.
    fn detach_orphaned_loops(&self, active_backings: &std::collections::HashSet<PathBuf>) {
        let loop_dir = format!("{}/swapfile", WORK_DIR);
        let Ok(entries) = std::fs::read_dir(&loop_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let fname = entry.file_name();
            if !fname.to_string_lossy().starts_with("loop_") {
                continue;
            }
            let Ok(content) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let mut lines = content.lines();
            let Some(loop_dev) = lines.next() else {
                continue;
            };
            let Some(backing_str) = lines.next() else {
                continue;
            };
            let backing = PathBuf::from(backing_str.trim());
            if !active_backings.contains(&backing) {
                info!(
                    "swapFC: detaching orphaned loop {} (backing {})",
                    loop_dev.trim(),
                    backing.display()
                );
                let _ = std::process::Command::new("losetup")
                    .args(["-d", loop_dev.trim()])
                    .status();
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Run the swap monitoring loop with controlled expansion/contraction
    ///
    /// Expansion: triggered ONLY by swap pressure (free_swap < free_swap_perc)
    /// This follows the proven approach from the old Python version:
    /// - The first file is always created at startup (min_count >= 1)
    /// - Additional files are created only when swap space is genuinely running low
    /// - With zswap, the kernel manages the RAM pool <-> disk writeback automatically,
    ///   so we only need to ensure there's enough disk-backed swap available
    ///
    /// Contraction: removes files when swap is abundant (free_swap > remove_free_swap_perc)
    pub fn run(&mut self) -> Result<()> {
        notify_ready();

        let use_loop = self.config.sparse_loop_backing;
        let mut loop_tick: u32 = 0;

        // Enforce readahead immediately after startup
        if use_loop {
            self.enforce_loop_readahead();
        }

        let mut retune_tick: u32 = 0;

        // Ensure minimum files are created at startup
        loop {
            let poll_interval = self.get_adaptive_poll_interval();
            thread::sleep(Duration::from_secs(poll_interval));

            if is_shutdown() {
                break;
            }

            // Periodically enforce readahead on loop devices (~every 5 ticks)
            // and re-apply all volatile queue params (~every 30 ticks)
            if use_loop {
                loop_tick += 1;
                retune_tick += 1;
                if loop_tick >= 5 {
                    loop_tick = 0;
                    self.enforce_loop_readahead();
                }
                if retune_tick >= 30 {
                    retune_tick = 0;
                    self.retune_all_loops();
                }
            }

            // Use zswap-aware swap calculation: pages in zswap RAM pool
            // are NOT consuming disk swap, so don't count them as "used"
            let free_swap = get_free_swap_percent_effective().unwrap_or(100);
            let free_ram = get_free_ram_percent().unwrap_or(100);

            // Get individual file statistics from /proc/swaps
            let swap_files = self.get_swapfiles_info();

            // Cooldown: prevent creating swapfiles too fast
            // ZSWAP: shorter cooldown since writeback consumes swapfiles quickly
            let cooldown_ok = self
                .last_creation
                .map(|t| t.elapsed() >= Duration::from_secs(self.cooldown_secs))
                .unwrap_or(true);

            // Emergency cooldown: short 5s for critical RAM/zswap situations
            let emergency_cooldown_ok = self
                .last_creation
                .map(|t| t.elapsed() >= Duration::from_secs(5))
                .unwrap_or(true);

            // Detect if swap is being actively consumed (free_swap dropped)
            // If so, the previous creation was justified — reset cooldown
            if free_swap < self.prev_free_swap.saturating_sub(5) {
                // Free swap dropped by more than 5% — swap is being consumed, reset cooldown
                self.cooldown_secs = 30;
            }
            self.prev_free_swap = free_swap;

            // ZSWAP SPARSE LOOP GROWTH STRATEGY:
            // Create a larger backing file when total disk swap is 80%+ full.
            //
            // IMPORTANT: must use DISK-based free swap, NOT `free_swap` (effective).
            // `get_free_swap_percent_effective()` adds Zswapped bytes (pages in zswap
            // RAM pool) back to free swap to avoid false disk-pressure alarms for
            // ZswapSwapfc.  For ZswapLoopfile (sparse files), that logic is wrong:
            // even though pages in the zswap pool haven't written to disk yet, their
            // swap slots are allocated, and the sparse blocks will be needed when the
            // shrinker evicts them.  Using effective free makes 99%-full files look
            // ~64% free and the growth trigger never fires.
            if self.config.sparse_loop_backing
                && !self.disk_full
                && self.allocated < self.config.max_count
            {
                // Compute free percentage from actual /proc/swaps usage of our files.
                let disk_free_swap: u8 = {
                    let total: u64 = swap_files.iter().map(|f| f.size_bytes).sum();
                    let used: u64 = swap_files.iter().map(|f| f.used_bytes).sum();
                    if total == 0 {
                        100
                    } else {
                        let free = total.saturating_sub(used);
                        ((free * 100) / total).min(100) as u8
                    }
                };

                if disk_free_swap < 20 && cooldown_ok {
                    let growth = if self.config.growth_chunk_size > 0 {
                        self.config.growth_chunk_size
                    } else {
                        self.config.chunk_size * 2
                    };
                    info!(
                        "swapFC: ZswapLoopfile disk swap 80%+ full (disk_free={}%, effective_free={}%) - creating growth file ({}MB)",
                        disk_free_swap,
                        free_swap,
                        growth / (1024 * 1024),
                    );
                    // Temporarily override chunk size for the next create call
                    let prev_chunk = self.config.chunk_size;
                    self.config.chunk_size = growth;
                    if self.create_swapfile().is_ok() {
                        self.last_creation = Some(Instant::now());
                        self.cooldown_secs = 30;
                    }
                    self.config.chunk_size = prev_chunk;
                    continue;
                }
            }

            // EXPANSION TRIGGERS (non-zswap only)
            // With zswap active, the reserve file strategy above handles ALL expansion.
            // The EMERGENCY and NORMAL triggers only apply to zram/plain swapfile modes.
            if !self.is_zswap_active
                && !self.disk_full
                && self.allocated < self.config.max_count
            {
                // Count files with no data yet to avoid pre-allocating more than needed
                let unused_count = swap_files.iter().filter(|f| f.used_bytes == 0).count();

                // EMERGENCY TRIGGER: critical RAM pressure.
                let emergency_ram_threshold: u8 = 10;

                if free_ram < emergency_ram_threshold
                    && free_swap < 80
                    && unused_count < 2
                    && emergency_cooldown_ok
                {
                    info!(
                        "swapFC: EMERGENCY! free_ram={}% free_swap={}% unused={} - creating swap urgently",
                        free_ram, free_swap, unused_count
                    );
                    if self.create_swapfile().is_ok() {
                        self.last_creation = Some(Instant::now());
                        self.cooldown_secs = 30;
                    }
                    continue;
                }

                let swap_threshold = self.config.free_swap_perc;

                // STRESS TRIGGER: existing files filling up (bypasses long cooldown).
                let files_stressed =
                    !swap_files.is_empty() && swap_files.iter().all(|f| f.usage_percent() >= 85);

                if files_stressed
                    && free_swap < swap_threshold
                    && unused_count < 2
                    && emergency_cooldown_ok
                {
                    info!(
                        "swapFC: all {} file(s) >= 85% full, free_swap={}% - expanding (stress trigger)",
                        swap_files.len(), free_swap
                    );
                    if self.create_swapfile().is_ok() {
                        self.last_creation = Some(Instant::now());
                        self.cooldown_secs = 30;
                    }
                    continue;
                }

                // NORMAL TRIGGER: swap space running low.
                if cooldown_ok && free_swap < swap_threshold && unused_count < 2 {
                    info!(
                        "swapFC: swap pressure! effective_free_swap={}% < {}% (thresh) - expanding (cooldown={}s)",
                        free_swap, swap_threshold, self.cooldown_secs
                    );
                    if self.create_swapfile().is_ok() {
                        self.last_creation = Some(Instant::now());
                        self.cooldown_secs = (self.cooldown_secs * 2).min(120);
                    }
                    continue;
                }
            }

            // CONTRACTION DECISION: check if swap is abundant enough to remove files
            if self.allocated > self.config.min_count {
                // ZSWAP: must always keep at least 2 unused reserve files.
                // Never remove if it would drop below the reserve threshold.
                if self.is_zswap_active {
                    let unused_count = swap_files.iter().filter(|f| f.used_bytes == 0).count();
                    if unused_count <= 2 {
                        // At or below minimum reserve — skip contraction
                        continue;
                    }
                }

                // ZSWAP: be conservative — swapfiles are writeback targets.
                let remove_threshold = if self.is_zswap_active {
                    85
                } else {
                    self.config.remove_free_swap_perc
                };

                // ZSWAP: 5 minutes minimum cooldown to prevent create-remove cycles
                let removal_cooldown_secs = if self.is_zswap_active { 300 } else { 60 };
                let removal_cooldown_ok = self
                    .last_creation
                    .map(|t| t.elapsed() >= Duration::from_secs(removal_cooldown_secs))
                    .unwrap_or(true);

                if free_swap > remove_threshold && removal_cooldown_ok {
                    if let Some(candidate) = self.find_safe_removal_candidate(&swap_files) {
                        info!(
                            "swapFC: free_swap={}% > {}% (thresh), removing {} (usage: {}%)",
                            free_swap,
                            remove_threshold,
                            candidate.path.display(),
                            candidate.usage_percent()
                        );
                        let path = candidate.path.clone();
                        if self.destroy_swapfile_by_path(&path).is_ok() {
                            self.disk_full = false; // Space freed, allow expansion again
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn get_adaptive_poll_interval(&self) -> u64 {
        if self.allocated > 0 {
            return self.config.frequency;
        }

        let free_ram = get_free_ram_percent().unwrap_or(100);

        if free_ram > 70 {
            10.min(self.config.frequency * 10)
        } else if free_ram > 50 {
            5.min(self.config.frequency * 5)
        } else if free_ram > self.config.free_ram_perc {
            2.min(self.config.frequency * 2)
        } else {
            self.config.frequency
        }
    }

    fn has_enough_space(&self, required_size: u64) -> bool {
        let check_path = self.config.path.clone();
        if let Ok(stat) = nix::sys::statvfs::statvfs(&check_path) {
            let free_bytes = stat.blocks_available() * stat.block_size();
            // Need at least 2x the required size (safety margin)
            free_bytes >= required_size * 2
        } else {
            false
        }
    }

    fn create_swapfile(&mut self) -> Result<()> {
        let next_file_num = self.allocated + 1;
        let chunk_size = self.config.chunk_size;

        if !self.has_enough_space(chunk_size) {
            if !self.disk_full {
                warn!(
                    "swapFC: ENOSPC (need {}MB) - pausing expansion",
                    chunk_size / (1024 * 1024)
                );
                self.disk_full = true;
            }
            return Err(SwapFileError::NoSpace);
        }

        notify_status(&format!(
            "Allocating swap file #{} ({}MB)...",
            next_file_num,
            chunk_size / (1024 * 1024)
        ));
        self.allocated += 1;
        self.file_sizes.push(chunk_size);

        let swapfile_path = self.config.path.join(self.allocated.to_string());

        // Remove if exists
        force_remove(&swapfile_path, false);

        // Create file with secure permissions (0600)
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&swapfile_path)?;
        }

        // NOCOW on btrfs — prevents deadlock under memory pressure.
        if self.is_btrfs && self.config.nocow {
            let _ = Command::new("chattr")
                .args(["+C"])
                .arg(&swapfile_path)
                .status();
        }

        // File allocation + optional loop device
        let (swapfile, loop_device): (String, Option<String>) = if self.config.sparse_loop_backing {
            // Sparse: allocate blocks on-demand via truncate.
            info!(
                "swapFC: creating sparse loop-backed file #{} ({}MB)",
                self.allocated,
                chunk_size / (1024 * 1024)
            );
            let status = Command::new("truncate")
                .args(["-s", &chunk_size.to_string()])
                .arg(&swapfile_path)
                .status()?;
            if !status.success() {
                force_remove(&swapfile_path, false);
                self.allocated -= 1;
                self.file_sizes.pop();
                return Err(SwapFileError::NoSpace);
            }
            // direct-io=on: bypasses page cache, prevents deadlock
            let loop_dev = run_cmd_output(&[
                "losetup",
                "-f",
                "--show",
                "--direct-io=on",
                &swapfile_path.to_string_lossy(),
            ])?;
            let loop_dev = loop_dev.trim().to_string();

            tune_loop_device(&loop_dev);

            (loop_dev.clone(), Some(loop_dev))
        } else {
            // Pre-allocate with zero-fill (direct swapon, no loop).
            // Cannot use fallocate on btrfs: it creates PREALLOC extents
            // that swapon rejects. Writing zeros creates REG extents.
            info!(
                "swapFC: creating preallocated file #{} ({}MB)",
                self.allocated,
                chunk_size / (1024 * 1024)
            );
            {
                use std::io::Write;
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&swapfile_path)?;
                let mut writer = std::io::BufWriter::with_capacity(1024 * 1024, f);
                let zeros = vec![0u8; 1024 * 1024];
                let chunks = chunk_size / (1024 * 1024);
                for _ in 0..chunks {
                    writer.write_all(&zeros)?;
                }
                let remainder = (chunk_size % (1024 * 1024)) as usize;
                if remainder > 0 {
                    writer.write_all(&vec![0u8; remainder])?;
                }
                writer.flush()?;
            }
            (swapfile_path.to_string_lossy().to_string(), None)
        };

        // mkswap
        let fs_label = if self.config.sparse_loop_backing {
            format!("SWAP_loop_{}", self.allocated)
        } else {
            format!("SWAP_btrfs_{}", self.allocated)
        };
        let status = Command::new("mkswap")
            .args(["-L", &fs_label])
            .arg(&swapfile)
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            force_remove(&swapfile_path, false);
            self.allocated -= 1;
            self.file_sizes.pop();
            return Err(SwapFileError::Io(std::io::Error::other("mkswap failed")));
        }

        // No discard for loop-backed swap on btrfs (PUNCH_HOLE destroys extents)
        let discard_options: Option<&str> = None;
        let unit_name = gen_swap_unit(
            Path::new(&swapfile),
            None,
            discard_options,
            &format!("swapfile_{}", self.allocated),
        )?;

        // Store loop device info for cleanup
        if let Some(ref loop_dev) = loop_device {
            let loop_info_path = format!("{}/swapfile/loop_{}", WORK_DIR, self.allocated);
            let _ = fs::write(
                &loop_info_path,
                format!("{}\n{}", loop_dev, swapfile_path.display()),
            );
        }

        systemctl(SystemctlAction::DaemonReload, "")?;
        systemctl(SystemctlAction::Start, &unit_name)?;

        // Re-apply volatile queue parameters that swapon may have reset.
        if let Some(ref loop_dev) = loop_device {
            std::thread::sleep(std::time::Duration::from_millis(100));
            retune_loop_queue(loop_dev);
        }

        notify_status("Monitoring memory status...");
        Ok(())
    }
}

/// Check if path is a btrfs subvolume
fn is_btrfs_subvolume(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }

    Command::new("btrfs")
        .args(["subvolume", "show"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
