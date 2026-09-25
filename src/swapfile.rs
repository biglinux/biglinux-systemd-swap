// SwapFC - Dynamic swap file management
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::config::Config;
use crate::defaults;
use crate::helpers::{
    force_remove, get_fstype, makedirs, parse_size as parse_size_shared, run_cmd_output,
};
use crate::meminfo::{
    get_effective_swap_usage, get_free_ram_percent, get_free_swap_percent_effective,
};
use crate::systemd::{activate_swap, gen_swap_unit, notify_ready, notify_status, swapoff};
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
}

/// Reject paths that point at critical system directories or are not absolute.
///
/// Accepts paths under `/`, `/var`, `/home`, `/swap`, `/mnt`, `/media`, `/tmp`,
/// `/run/user` and similar writable locations. Rejects bare system directories
/// such as `/etc`, `/sys`, `/proc`, `/dev`, `/bin`, `/sbin`, `/usr`, `/lib`,
/// `/boot`, and `/run` itself.
pub(crate) fn validate_swapfile_path(path: &Path) -> bool {
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
        let path = config
            .get("swapfile_path")
            .unwrap_or(defaults::SWAPFILE_PATH)
            .to_string();
        let path = PathBuf::from(path.trim_end_matches('/'));
        if !validate_swapfile_path(&path) {
            return Err(SwapFileError::InvalidPath);
        }

        let chunk_size_str = config
            .get("swapfile_chunk_size")
            .unwrap_or(defaults::SWAPFILE_CHUNK_SIZE)
            .to_string();
        let chunk_size =
            parse_size_shared(&chunk_size_str).map_err(|_| SwapFileError::InvalidPath)?;
        let chunk_size = chunk_size.max(512 * 1024 * 1024);

        let max_count: u32 = config
            .get_as("swapfile_max_count")
            .unwrap_or(defaults::SWAPFILE_MAX_COUNT);
        let max_count = max_count.clamp(1, 28);

        let min_count: u32 = config
            .get_as("swapfile_min_count")
            .unwrap_or(defaults::SWAPFILE_MIN_COUNT);
        let frequency: u64 = config
            .get_as::<u32>("swapfile_frequency")
            .unwrap_or(defaults::SWAPFILE_FREQUENCY) as u64;
        let frequency = frequency.clamp(1, 86400);

        // Clamp while still u32: `as u8` first wraps, so 300 became 44
        // instead of the documented ceiling.
        let percent = |key: &str, default: u8, min: u32, max: u32| -> u8 {
            config
                .get_as::<u32>(key)
                .unwrap_or(default as u32)
                .clamp(min, max) as u8
        };
        let shrink_threshold = percent(
            "swapfile_shrink_threshold",
            defaults::SWAPFILE_SHRINK_THRESHOLD,
            10,
            50,
        );
        let safe_headroom = percent(
            "swapfile_safe_headroom",
            defaults::SWAPFILE_SAFE_HEADROOM,
            20,
            60,
        );

        Ok(Self {
            path,
            chunk_size,
            max_count,
            min_count,
            free_ram_perc: percent(
                "swapfile_free_ram_perc",
                defaults::SWAPFILE_FREE_RAM_PERC,
                0,
                100,
            ),
            free_swap_perc: percent(
                "swapfile_free_swap_perc",
                defaults::SWAPFILE_FREE_SWAP_PERC,
                0,
                100,
            ),
            remove_free_swap_perc: percent(
                "swapfile_remove_free_swap_perc",
                defaults::SWAPFILE_REMOVE_FREE_SWAP_PERC,
                0,
                100,
            ),
            frequency,
            shrink_threshold,
            safe_headroom,
        })
    }
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
            "swapFC: chunk={}MB",
            swapfile_config.chunk_size / (1024 * 1024)
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
                // Clear the way only when there is nothing to lose. A directory
                // with entries in it is holding live swap: the swapfiles this
                // very instance is about to adopt a few lines further on, and
                // any zram writeback store an administrator pointed here. The
                // snapshot isolation a subvolume adds is not worth destroying
                // those for, so an occupied directory is kept as it stands --
                // `btrfs subvolume create` then fails on it and the fallback
                // below carries on with a plain directory, NOCOW included.
                let occupied = swapfile_config
                    .path
                    .read_dir()
                    .map(|mut entries| entries.next().is_some())
                    .unwrap_or(false);

                if swapfile_config.path.exists() && !occupied {
                    warn!("swapFC: path exists but not a subvolume, removing...");
                    if swapfile_config.path.is_dir() {
                        fs::remove_dir_all(&swapfile_config.path)?;
                    } else {
                        fs::remove_file(&swapfile_config.path)?;
                    }
                } else if occupied {
                    warn!(
                        "swapFC: {:?} holds files and is not a subvolume; keeping it \
                         (no snapshot isolation)",
                        swapfile_config.path
                    );
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

                    info!(
                        "swapFC: created directory (non-subvolume) at {:?}",
                        swapfile_config.path
                    );
                } else {
                    info!(
                        "swapFC: created btrfs subvolume at {:?}",
                        swapfile_config.path
                    );
                }
            }

            // NOCOW on the directory, so every file created in it inherits it:
            // btrfs refuses swapon on a copy-on-write file, and block allocation
            // during swap writes can deadlock under memory pressure.
            let _ = Command::new("chattr")
                .args(["+C"])
                .arg(&swapfile_config.path)
                .status();
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
                "swapFC: ZSWAP mode enabled - initial_count={} chunk={}MB",
                self.config.min_count,
                self.config.chunk_size / (1024 * 1024)
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

            // The kernel prints the path as seen from the mount namespace
            // that ran swapon. Our swapon runs in this unit's namespace, where
            // the swap directory is a mount of its own, so a file a previous
            // instance activated reads `/1` once that instance has exited.
            // Without this the next instance saw none of its files, called
            // them stale and could neither delete nor recreate them.
            let mut path = PathBuf::from(fields[0]);
            if !path.starts_with(&self.config.path) {
                let name = fields[0].trim_start_matches('/');
                if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                    continue; // not ours: only our files are numbered
                }
                path = self.config.path.join(name);
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
        files.sort_by_key(|f| std::cmp::Reverse(f.priority));
        files
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
        candidates.sort_by_key(|c| c.priority);

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

        force_remove(path, false);

        // Clean up systemd unit
        if let Some(idx) = file_index {
            // The whole tag line: a substring test for `swapfile_1` also
            // matched `swapfile_12` and deleted that file's unit instead.
            let tag_line = format!("# Tag=swapfile_{}", idx);
            for unit_path in crate::helpers::find_swap_units() {
                if let Ok(content) = crate::helpers::read_file(&unit_path) {
                    if content.lines().any(|line| line.trim() == tag_line) {
                        force_remove(&unit_path, true);
                        break;
                    }
                }
            }

            // Update file_sizes if we tracked this file.
            // Guard against idx==0 (would underflow (idx-1) as usize).
            if idx > 0 && idx <= self.file_sizes.len() as u32 {
                self.file_sizes.remove((idx - 1) as usize);
            }
        }

        self.allocated = self.allocated.saturating_sub(1);

        info!("swapFC: {} removed successfully", path.display());
        notify_status("Monitoring memory status...");
        Ok(())
    }

    /// Index of one of our swap files: its file name.
    fn find_file_index(&self, path: &Path) -> Option<u32> {
        if !path.starts_with(&self.config.path) {
            return None;
        }
        path.file_name()?.to_string_lossy().parse().ok()
    }

    /// Adopt swap files that already exist from a previous run.
    /// Called before create_initial_swap() so we never swapoff active files on restart.
    fn adopt_existing_swapfiles(&mut self) {
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

    /// Remove empty adopted swapfiles above min_count at startup (no cooldown).
    /// Iterates lowest-priority (last created) first for cleanest teardown order.
    fn shed_excess_empty_adopted(&mut self) {
        // The same RAM guard as the monitor's contraction. A restart no longer
        // swaps anything off, so it can land under load: measured on the
        // notebook, file #2 went at 4% free RAM and the emergency path created
        // it again seconds later.
        let free_ram = get_free_ram_percent().unwrap_or(100);
        if free_ram <= self.config.free_ram_perc {
            return;
        }
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

        let active: std::collections::HashSet<PathBuf> =
            active_swaps.iter().map(|f| f.path.clone()).collect();

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

        // Woken early when memory pressure starts; see `PressureWait`.
        let pressure = crate::meminfo::PressureWait::new();

        // Ensure minimum files are created at startup
        loop {
            let poll_interval = self.get_adaptive_poll_interval();
            pressure.wait(Duration::from_secs(poll_interval));

            if is_shutdown() {
                break;
            }

            // Use zswap-aware swap calculation: pages in zswap RAM pool
            // are NOT consuming disk swap, so don't count them as "used"
            let free_swap = get_free_swap_percent_effective().unwrap_or(100);
            let free_ram = get_free_ram_percent().unwrap_or(100);

            // Get individual file statistics from /proc/swaps
            let swap_files = self.get_swapfiles_info();

            // How full OUR files are, from their own /proc/swaps rows.
            //
            // `free_swap` above is a fraction of SwapTotal, and SwapTotal is
            // dominated by zram: at a zram disksize of 150% of RAM it is 46 GB
            // of the 47 GB total, so the figure reports how empty zram is and
            // says nothing about disk. Two decisions below were reading it and
            // both were wrong in the same direction. Growth asked for
            // `free_swap` under 40%, which needs zram 60% full -- around 28 GB
            // of compressed pages, more than a 31 GB host can hold -- so it
            // never fired and only the RAM emergency path ever created a file.
            // Removal asked for `free_swap` above 70%, which is true whenever
            // zram is under 30% full, so it deleted the disk chunk while RAM
            // was already short. Observed on one host: the last chunk went at
            // 13:57:06 on a `free_swap=71%` reading and the reclaim deadlock
            // followed at 15:23:15 with 512 MB of disk swap left.
            let disk_free_percent: u8 = {
                let total: u64 = swap_files.iter().map(|f| f.size_bytes).sum();
                let used: u64 = swap_files.iter().map(|f| f.used_bytes).sum();
                if total == 0 {
                    100
                } else {
                    ((total.saturating_sub(used) * 100) / total).min(100) as u8
                }
            };

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

            // EXPANSION TRIGGERS, in every mode. With zswap each stored page
            // holds a slot in these files, so their fill is what zswap uses up.
            // Zswap mode used to skip all of this and grow only through the
            // removed sparse loop path, which left it at `min_count` files.
            if !self.disk_full && self.allocated < self.config.max_count {
                // Count files with no data yet to avoid pre-allocating more than needed
                let unused_count = swap_files.iter().filter(|f| f.used_bytes == 0).count();

                // EMERGENCY TRIGGER: critical RAM pressure.
                let emergency_ram_threshold: u8 = 10;

                // Low RAM alone does not justify a disk file. These files sit
                // at priority -1, below the zram tier, so the kernel reaches
                // them only once zram is full -- while real swap headroom
                // remains, every page goes to zram and the file we create stays
                // at Used=0. `unused_count` does not stop this: contraction
                // deletes the idle file, and the next tick creates it again, so
                // the pair flaps under sustained pressure (observed: 9 creates,
                // 7 deletes in one boot, none ever used). Gate on the whole
                // swap stack, not zram's share of it: create only when kernel
                // SwapFree falls under two chunks, i.e. zram itself is nearly
                // spent and the disk tier is about to be the one in use.
                let swap_headroom = get_effective_swap_usage()
                    .map(|u| u.swap_free)
                    .unwrap_or(u64::MAX);
                let swap_nearly_full = swap_headroom < self.config.chunk_size.saturating_mul(2);
                if free_ram < emergency_ram_threshold
                    && swap_nearly_full
                    && unused_count < 2
                    && emergency_cooldown_ok
                {
                    info!(
                        "swapFC: EMERGENCY! free_ram={}% disk_free={}% unused={} - creating swap urgently",
                        free_ram, disk_free_percent, unused_count
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

                // `files_stressed` already means every file we own is 85% full,
                // so the extra `free_swap` term added nothing but the chance to
                // veto a justified expansion on a reading about zram.
                if files_stressed && unused_count < 2 && emergency_cooldown_ok {
                    info!(
                        "swapFC: all {} file(s) >= 85% full, disk_free={}% - expanding (stress trigger)",
                        swap_files.len(), disk_free_percent
                    );
                    if self.create_swapfile().is_ok() {
                        self.last_creation = Some(Instant::now());
                        self.cooldown_secs = 30;
                    }
                    continue;
                }

                // NORMAL TRIGGER: our own disk swap running low.
                if cooldown_ok && disk_free_percent < swap_threshold && unused_count < 2 {
                    info!(
                        "swapFC: disk swap pressure! disk_free={}% < {}% (thresh) - expanding (cooldown={}s)",
                        disk_free_percent, swap_threshold, self.cooldown_secs
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

                // Shed a file only when OUR files are the thing that is idle,
                // and only while RAM is still comfortable. The reading used to
                // be `free_swap`, so a mostly-empty zram read as abundant swap
                // and the chunk went away exactly when RAM was getting short.
                let ram_comfortable = free_ram > self.config.free_ram_perc;
                if disk_free_percent > remove_threshold && ram_comfortable && removal_cooldown_ok {
                    if let Some(candidate) = self.find_safe_removal_candidate(&swap_files) {
                        info!(
                            "swapFC: disk_free={}% > {}% (thresh), free_ram={}%, removing {} (usage: {}%)",
                            disk_free_percent,
                            remove_threshold,
                            free_ram,
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

        // One cleanup for every failure: a partial file would hold disk space
        // and the counters would name a swap file that does not exist.
        if let Err(e) = self.allocate_file(&swapfile_path, chunk_size) {
            {
                force_remove(&swapfile_path, false);
                self.allocated -= 1;
                self.file_sizes.pop();
                return Err(e);
            }
        }
        let swapfile = swapfile_path.to_string_lossy().to_string();

        // mkswap
        let fs_label = format!("SWAP_btrfs_{}", self.allocated);
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

        let unit_name = gen_swap_unit(
            Path::new(&swapfile),
            None,
            None,
            &format!("swapfile_{}", self.allocated),
        )?;

        activate_swap(&swapfile, None, false, &unit_name)?;

        notify_status("Monitoring memory status...");
        Ok(())
    }

    /// Create `swapfile_path` and reserve `chunk_size` bytes for it. The caller
    /// removes the file on error.
    fn allocate_file(&self, swapfile_path: &Path, chunk_size: u64) -> Result<()> {
        // Create file with secure permissions (0600)
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(swapfile_path)?;
        }

        // NOCOW on btrfs — prevents deadlock under memory pressure.
        if self.is_btrfs {
            let _ = Command::new("chattr")
                .args(["+C"])
                .arg(swapfile_path)
                .status();
        }

        // Btrfs supports preallocated NOCOW swapfiles. Avoid writing the
        // entire file under memory pressure just to reserve its extents.
        info!(
            "swapFC: creating preallocated file #{} ({}MB)",
            self.allocated,
            chunk_size / (1024 * 1024)
        );
        if self.is_btrfs {
            run_cmd_output(&[
                "fallocate",
                "--length",
                &chunk_size.to_string(),
                "--",
                &swapfile_path.to_string_lossy(),
            ])?;
        } else {
            use std::io::Write;
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(swapfile_path)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── SwapFileInfo ─────────────────────────────────────────────────────────

    fn info(size: u64, used: u64) -> SwapFileInfo {
        SwapFileInfo {
            path: PathBuf::from("/swapfile/1"),
            size_bytes: size,
            used_bytes: used,
            priority: 0,
        }
    }

    #[test]
    fn usage_percent_zero_size_is_zero() {
        assert_eq!(info(0, 0).usage_percent(), 0);
    }

    #[test]
    fn usage_percent_half_full() {
        assert_eq!(info(1000, 500).usage_percent(), 50);
    }

    #[test]
    fn usage_percent_full() {
        assert_eq!(info(1000, 1000).usage_percent(), 100);
    }

    #[test]
    fn is_nearly_empty_true_below_threshold() {
        assert!(info(1000, 100).is_nearly_empty(30));
    }

    #[test]
    fn is_nearly_empty_equal_threshold_true() {
        // Uses <= for boundary
        assert!(info(1000, 300).is_nearly_empty(30));
    }

    #[test]
    fn is_nearly_empty_false_above_threshold() {
        assert!(!info(1000, 500).is_nearly_empty(30));
    }

    // ── validate_swapfile_path ───────────────────────────────────────────────

    #[test]
    fn validate_rejects_relative_path() {
        assert!(!validate_swapfile_path(Path::new("relative/path")));
        assert!(!validate_swapfile_path(Path::new("swapfile")));
    }

    #[test]
    fn validate_rejects_forbidden_system_dirs() {
        for p in &[
            "/etc",
            "/etc/swap",
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
            "/usr/local/swap",
            "/boot/swap",
        ] {
            assert!(
                !validate_swapfile_path(Path::new(p)),
                "{} should be rejected",
                p
            );
        }
    }

    #[test]
    fn validate_accepts_normal_swap_paths() {
        for p in &[
            "/swapfile",
            "/swap",
            "/var/swapfile",
            "/home/swap",
            "/mnt/swap",
            "/tmp/swap",
        ] {
            assert!(
                validate_swapfile_path(Path::new(p)),
                "{} should be accepted",
                p
            );
        }
    }

    #[test]
    fn validate_rejects_prefix_only_when_full_component() {
        // "/etcd" must NOT be rejected even though it starts with "/etc".
        // The validator checks "/etc/" (with trailing slash) or exact "/etc".
        assert!(validate_swapfile_path(Path::new("/etcd")));
        assert!(validate_swapfile_path(Path::new("/developer")));
    }

    // ── SwapFileConfig::from_config ──────────────────────────────────────────

    fn cfg(pairs: &[(&str, &str)]) -> crate::config::Config {
        crate::config::Config::from_pairs_for_tests(pairs.iter().map(|(k, v)| (*k, *v)))
    }

    #[test]
    fn from_config_rejects_invalid_path() {
        let c = cfg(&[("swapfile_path", "/etc")]);
        assert!(matches!(
            SwapFileConfig::from_config(&c),
            Err(SwapFileError::InvalidPath)
        ));
    }

    #[test]
    fn from_config_clamps_max_count_to_28() {
        for (v, want) in [("28", 28), ("29", 28), ("99", 28)] {
            let c = cfg(&[("swapfile_max_count", v)]);
            assert_eq!(SwapFileConfig::from_config(&c).unwrap().max_count, want);
        }
    }

    #[test]
    fn from_config_clamps_max_count_min_1() {
        let c = cfg(&[("swapfile_max_count", "0")]);
        let sc = SwapFileConfig::from_config(&c).unwrap();
        assert_eq!(sc.max_count, 1);
    }

    #[test]
    fn from_config_clamps_shrink_threshold_10_50() {
        for (v, want) in [("5", 10), ("10", 10), ("50", 50)] {
            let c = cfg(&[("swapfile_shrink_threshold", v)]);
            assert_eq!(
                SwapFileConfig::from_config(&c).unwrap().shrink_threshold,
                want
            );
        }

        // 300 used to wrap through `as u8` to 44 before the clamp saw it.
        for v in ["99", "300"] {
            let c = cfg(&[("swapfile_shrink_threshold", v)]);
            assert_eq!(
                SwapFileConfig::from_config(&c).unwrap().shrink_threshold,
                50
            );
        }
    }

    #[test]
    fn from_config_clamps_safe_headroom_20_60() {
        for (v, want) in [("5", 20), ("20", 20), ("60", 60), ("99", 60)] {
            let c = cfg(&[("swapfile_safe_headroom", v)]);
            assert_eq!(SwapFileConfig::from_config(&c).unwrap().safe_headroom, want);
        }
    }

    #[test]
    fn from_config_clamps_frequency_1_86400() {
        let c = cfg(&[("swapfile_frequency", "0")]);
        assert_eq!(SwapFileConfig::from_config(&c).unwrap().frequency, 1);
    }

    #[test]
    fn from_config_enforces_min_chunk() {
        let mb = 1024 * 1024;
        for (v, want) in [("64M", 512 * mb), ("512M", 512 * mb), ("2G", 2048 * mb)] {
            let c = cfg(&[("swapfile_chunk_size", v)]);
            assert_eq!(SwapFileConfig::from_config(&c).unwrap().chunk_size, want);
        }
    }

    #[test]
    fn from_config_trailing_slash_stripped() {
        let c = cfg(&[("swapfile_path", "/swap/")]);
        let sc = SwapFileConfig::from_config(&c).unwrap();
        assert_eq!(sc.path, PathBuf::from("/swap"));
    }
}
