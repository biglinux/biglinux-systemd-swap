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

const MB: u64 = 1024 * 1024;

/// SwapFree under which the disk tier grows while RAM is short: 15% of
/// SwapTotal, 5 points clear of systemd-oomd's default `SwapUsedLimit=90%`,
/// and never under two chunks, which is what small hosts reach first.
fn headroom_floor(total: u64, chunk: u64) -> u64 {
    (total / 100 * 15).max(chunk.saturating_mul(2))
}

/// Whether swap held still between two checks, each read as (free RAM %,
/// effective SwapFree MB, used MB of our files): RAM within a point, swap
/// within 64 MB.
fn swap_held_still(last: Option<(u8, u64, u64)>, now: (u8, u64, u64)) -> bool {
    last.is_some_and(|(ram, free, used)| {
        ram.abs_diff(now.0) <= 1 && free.abs_diff(now.1) < 64 && used.abs_diff(now.2) < 64
    })
}

/// Size of one file that brings SwapFree back to twice `floor`, never below
/// `chunk`, rounded up to whole MiB for mkswap.
///
/// Twice the floor, not a share of SwapTotal: on the 4 GB notebook the floor is
/// the two-chunk minimum, a 25% share sat under it, and the file came out at
/// one chunk -- 512 MB against random data arriving at 128 MB/s, with each
/// create taking 9 s on the saturated HDD. oomd killed the load 20 s later.
fn headroom_file_size(floor: u64, free: u64, chunk: u64) -> u64 {
    floor
        .saturating_mul(2)
        .saturating_sub(free)
        .div_ceil(MB)
        .saturating_mul(MB)
        .max(chunk)
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
    /// Free space to keep on the filesystem, as a percentage ("5%") or a size
    /// ("2G"). None resolves to a percentage clamped into an absolute band.
    pub min_free_disk: Option<MinFree>,
}

/// An explicit `swapfile_min_free_disk`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MinFree {
    Percent(u64),
    Bytes(u64),
}

/// Parse a percentage of the filesystem ("5%", up to 100) or a size ("2G").
pub fn parse_min_free(s: &str) -> Option<MinFree> {
    let s = s.trim();
    match s.strip_suffix('%') {
        Some(pct) => pct
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&p| p <= 100)
            .map(MinFree::Percent),
        None => parse_size_shared(s).ok().map(MinFree::Bytes),
    }
}

/// Free space to keep on the filesystem after allocating a swap file.
///
/// An explicit value is taken as given; only the automatic default is clamped,
/// since an admin asking for a specific reserve has already made the judgement.
pub fn resolve_min_free(fs_total: u64, configured: Option<MinFree>) -> u64 {
    // Multiply before dividing, in u128 so a filesystem large enough to
    // overflow the intermediate cannot exist.
    let percent_of_total = |p: u64| -> u64 { ((fs_total as u128 * p as u128) / 100) as u64 };

    match configured {
        Some(MinFree::Percent(p)) => percent_of_total(p),
        Some(MinFree::Bytes(b)) => b,
        None => percent_of_total(defaults::SWAPFILE_MIN_FREE_PERCENT).clamp(
            defaults::SWAPFILE_MIN_FREE_FLOOR,
            defaults::SWAPFILE_MIN_FREE_CAP,
        ),
    }
}

/// Bytes not yet allocated to any btrfs chunk, from `btrfs filesystem usage -b`.
pub fn parse_btrfs_unallocated(output: &str) -> Option<u64> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Device unallocated:")
            .and_then(|rest| rest.trim().parse().ok())
    })
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
            min_free_disk: {
                let s = config.get("swapfile_min_free_disk").unwrap_or("").trim();
                let parsed = parse_min_free(s);
                if parsed.is_none() && !s.is_empty() {
                    warn!(
                        "swapFC: swapfile_min_free_disk={:?} is neither a percentage nor a size, \
                         keeping the automatic reserve",
                        s
                    );
                }
                parsed
            },
        })
    }
}

/// SwapFC manager - supports btrfs, ext4, and xfs
pub struct SwapFile {
    config: SwapFileConfig,
    /// How many of our files are active. Not the highest index: contraction
    /// removes whichever file is safest, so the numbers in use have gaps.
    allocated: u32,
    /// True if path is on btrfs (for subvolume/nodatacow handling)
    is_btrfs: bool,
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

            if !is_subvolume && swapfile_config.path.is_dir() {
                // An existing directory is kept as it stands, empty or not. It
                // may hold the swap files this instance is about to adopt, or a
                // zram writeback store, and under ProtectSystem=strict a path in
                // ReadWritePaths= is a bind mount that cannot be removed anyway.
                // It goes without the snapshot isolation a subvolume adds; the
                // NOCOW flag below is what swapon needs.
                info!(
                    "swapFC: {:?} is a directory, not a subvolume; using it as it is",
                    swapfile_config.path
                );
            } else if !is_subvolume {
                if swapfile_config.path.exists() {
                    warn!(
                        "swapFC: {:?} is not a directory, removing it",
                        swapfile_config.path
                    );
                    fs::remove_file(&swapfile_config.path)?;
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
        let count = self
            .get_swapfiles_info()
            .iter()
            .filter(|f| self.find_file_index(&f.path).is_some())
            .count() as u32;
        if count > 0 {
            info!("swapFC: adopting {} existing file(s)", count);
            self.allocated = count;
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
            if let Err(e) = self.create_swapfile(self.config.chunk_size) {
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
        let mut backoff =
            crate::meminfo::Backoff::new(Duration::from_secs(self.config.frequency), &pressure);
        let mut settled = false;
        // (free RAM %, effective SwapFree MB, used MB of our files) at the last check.
        let mut last_reading: Option<(u8, u64, u64)> = None;

        loop {
            let woke = pressure.wait(backoff.next(settled));

            if is_shutdown() {
                break;
            }

            // Use zswap-aware swap calculation: pages in zswap RAM pool
            // are NOT consuming disk swap, so don't count them as "used"
            let free_swap = get_free_swap_percent_effective().unwrap_or(100);
            let free_ram = get_free_ram_percent().unwrap_or(100);

            // Get individual file statistics from /proc/swaps
            let swap_files = self.get_swapfiles_info();

            let usage = get_effective_swap_usage().ok();

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

            // Settled: RAM within a point and swap within 64 MB of the last
            // check, and every expansion trigger clear by a margin, since a
            // long wait leaves kswapd room to fill a file without a stall:
            // RAM more than 5 points above `free_ram_perc`, SwapFree above
            // twice the headroom floor, our files at least 10 points above
            // `free_swap_perc` free and not all 75% full.
            let reading = (
                free_ram,
                usage.as_ref().map_or(0, |u| u.swap_free / MB),
                swap_files.iter().map(|f| f.used_bytes).sum::<u64>() / MB,
            );
            let far_from_triggers = free_ram > self.config.free_ram_perc.saturating_add(5)
                && usage.as_ref().is_some_and(|u| {
                    u.swap_free > 2 * headroom_floor(u.swap_total, self.config.chunk_size)
                })
                && disk_free_percent >= self.config.free_swap_perc.saturating_add(10)
                && (swap_files.is_empty() || swap_files.iter().any(|f| f.usage_percent() < 75));
            settled = !woke && far_from_triggers && swap_held_still(last_reading, reading);
            last_reading = Some(reading);

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

                // HEADROOM TRIGGER: the whole swap stack is running out while
                // RAM is short. systemd-oomd's `ManagedOOMSwap=kill` acts once
                // MemAvailable and SwapFree are both under 10% of their totals
                // (SwapUsedLimit=90%), so the disk tier has to grow before that
                // line, not after. The old gate -- free RAM under 10% and
                // SwapFree under two chunks -- sat past it: 1 GB of a 16.8 GB
                // stack is 94% used, and on the reference host oomd killed the
                // browser in the same second the emergency file was created.
                //
                // Both halves are needed. These files sit at priority -1, below
                // zram, so while the stack has headroom every page goes to zram
                // and a new file stays at Used=0; contraction then deletes it
                // and the next tick recreates it (observed: 9 creates, 7 deletes
                // in one boot, none ever used). Keying on the whole stack keeps
                // the file for when zram is nearly spent, and keying on RAM at
                // `free_ram_perc` -- the same line contraction needs RAM above
                // before it removes anything -- keeps the two from flapping.
                let (swap_total, swap_free) = usage
                    .as_ref()
                    .map(|u| (u.swap_total, u.swap_free))
                    .unwrap_or((0, u64::MAX));
                let floor = headroom_floor(swap_total, self.config.chunk_size);
                if free_ram <= self.config.free_ram_perc
                    && swap_free < floor
                    && unused_count < 2
                    && emergency_cooldown_ok
                {
                    // One file sized to restore the headroom, not a 512 MB
                    // chunk every 5 s: the reference load moved 4 GB into swap
                    // in 30 s. Only where the file is preallocated -- zero-filling
                    // gigabytes elsewhere would add writeback under pressure.
                    let refill = headroom_file_size(floor, swap_free, self.config.chunk_size);
                    let size = if self.is_btrfs && self.has_enough_space(refill) {
                        refill
                    } else {
                        self.config.chunk_size
                    };
                    info!(
                        "swapFC: swap headroom low! free_ram={}% swap_free={}MB/{}MB - creating {}MB",
                        free_ram,
                        swap_free / MB,
                        swap_total / MB,
                        size / MB
                    );
                    settled = false;
                    if self.create_swapfile(size).is_ok() {
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
                    settled = false;
                    if self.create_swapfile(self.config.chunk_size).is_ok() {
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
                    settled = false;
                    if self.create_swapfile(self.config.chunk_size).is_ok() {
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
                        settled = false;
                        if self.destroy_swapfile_by_path(&path).is_ok() {
                            self.disk_full = false; // Space freed, allow expansion again
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Whether a file of `required_size` can be created while leaving the
    /// filesystem with enough room to keep working.
    ///
    /// The old rule was free >= required * 2, which scales with the swap file
    /// rather than with the filesystem, so it let allocation continue until
    /// roughly one chunk remained free. What matters instead is what survives
    /// the allocation, so the reserve is the thing that has to still be there
    /// afterwards.
    fn has_enough_space(&self, required_size: u64) -> bool {
        let Ok(stat) = nix::sys::statvfs::statvfs(&self.config.path) else {
            return false;
        };

        let block_size = stat.block_size();
        let free = stat.blocks_available() * block_size;
        let total = stat.blocks() * block_size;
        let reserve = resolve_min_free(total, self.config.min_free_disk);

        if free < required_size.saturating_add(reserve) {
            return false;
        }

        // btrfs reports space inside allocated data chunks as free even when
        // nothing is left to carve a metadata chunk from. Writing into that
        // window is how a filesystem ends up ENOSPC with free space showing,
        // so ask btrfs directly rather than trusting statvfs alone.
        if self.is_btrfs {
            if let Ok(out) = run_cmd_output(&[
                "btrfs",
                "filesystem",
                "usage",
                "-b",
                &self.config.path.to_string_lossy(),
            ]) {
                if let Some(unallocated) = parse_btrfs_unallocated(&out) {
                    if unallocated < defaults::SWAPFILE_BTRFS_MIN_UNALLOCATED {
                        warn!(
                            "swapFC: btrfs has only {}MB unallocated - refusing to allocate",
                            unallocated / (1024 * 1024)
                        );
                        return false;
                    }
                }
            }
        }

        true
    }

    fn create_swapfile(&mut self, chunk_size: u64) -> Result<()> {
        // The lowest number no active file of ours holds. `allocated + 1` was
        // used before, and after contraction left only `/swapfile/3` a restart
        // counted three files and shed the one real file as surplus.
        let in_use: Vec<u32> = self
            .get_swapfiles_info()
            .iter()
            .filter_map(|f| self.find_file_index(&f.path))
            .collect();
        let index = (1..).find(|n| !in_use.contains(n)).unwrap_or(1);

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
            index,
            chunk_size / (1024 * 1024)
        ));
        self.allocated += 1;

        let swapfile_path = self.config.path.join(index.to_string());

        // Remove if exists
        force_remove(&swapfile_path, false);

        // One cleanup for every failure: a partial file would hold disk space
        // and the counters would name a swap file that does not exist.
        info!(
            "swapFC: creating preallocated file #{} ({}MB)",
            index,
            chunk_size / MB
        );
        if let Err(e) = self.allocate_file(&swapfile_path, chunk_size) {
            {
                force_remove(&swapfile_path, false);
                self.allocated -= 1;
                return Err(e);
            }
        }
        let swapfile = swapfile_path.to_string_lossy().to_string();

        // mkswap
        let fs_label = format!("SWAP_btrfs_{}", index);
        let status = Command::new("mkswap")
            .args(["-L", &fs_label])
            .arg(&swapfile)
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            force_remove(&swapfile_path, false);
            self.allocated -= 1;
            return Err(SwapFileError::Io(std::io::Error::other("mkswap failed")));
        }

        let unit_name = gen_swap_unit(
            Path::new(&swapfile),
            None,
            None,
            &format!("swapfile_{}", index),
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

    // ── Free space reserve ───────────────────────────────────────────────────

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn reserve_is_a_percentage_between_the_bounds() {
        assert_eq!(resolve_min_free(100 * GIB, None), 5 * GIB);
    }

    #[test]
    fn reserve_floors_on_small_filesystems() {
        // 5% of 8 GiB is 409 MiB, under one btrfs metadata chunk plus its
        // global reserve, so the floor takes over.
        assert_eq!(resolve_min_free(8 * GIB, None), GIB);
        assert_eq!(resolve_min_free(GIB, None), GIB);
    }

    #[test]
    fn reserve_caps_on_large_filesystems() {
        // 5% of 4 TiB would withhold 200 GiB for no additional safety.
        assert_eq!(resolve_min_free(4096 * GIB, None), 8 * GIB);
    }

    #[test]
    fn reserve_honours_an_explicit_size_or_percentage() {
        assert_eq!(resolve_min_free(100 * GIB, parse_min_free("2G")), 2 * GIB);
        assert_eq!(resolve_min_free(100 * GIB, parse_min_free("10%")), 10 * GIB);
        // An explicit value is not clamped: the admin has made the call.
        assert_eq!(
            resolve_min_free(4096 * GIB, parse_min_free("50%")),
            2048 * GIB
        );
        assert_eq!(
            resolve_min_free(100 * GIB, parse_min_free("64M")),
            64 * 1024 * 1024
        );
    }

    #[test]
    fn reserve_rejects_nonsense() {
        assert_eq!(parse_min_free("banana"), None);
        assert_eq!(parse_min_free("%"), None);
        assert_eq!(parse_min_free("101%"), None);
        assert_eq!(parse_min_free(""), None);
    }

    // ── btrfs unallocated ────────────────────────────────────────────────────

    #[test]
    fn parses_device_unallocated_from_btrfs_usage() {
        let out = "\
Overall:
    Device size:                 999930462208
    Device allocated:            901976686592
    Device unallocated:           97953775616
    Used:                        870399021056
    Free (estimated):            120930316288      (min: 120930316288)
";
        assert_eq!(parse_btrfs_unallocated(out), Some(97953775616));
    }

    #[test]
    fn btrfs_unallocated_absent_when_unparseable() {
        assert_eq!(parse_btrfs_unallocated(""), None);
        // Human-readable output (without -b) is not a number we can use.
        assert_eq!(
            parse_btrfs_unallocated("    Device unallocated:\t  91.23GiB\n"),
            None
        );
    }

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
    fn swap_held_still_thresholds() {
        // swap.conf(5): a point of RAM, 64 MB of swap.
        assert!(!swap_held_still(None, (50, 1000, 100)));
        assert!(swap_held_still(Some((50, 1000, 100)), (51, 1063, 37)));
        assert!(!swap_held_still(Some((50, 1000, 100)), (52, 1000, 100)));
        assert!(!swap_held_still(Some((50, 1000, 100)), (50, 1064, 100)));
        assert!(!swap_held_still(Some((50, 1000, 100)), (50, 1000, 164)));
    }

    #[test]
    fn headroom_file_doubles_the_floor() {
        // The 2026-09-27 kill: 16.8 GB of swap with 1 GB free. 15% binds.
        let (total, free) = (16_875_577_344u64, 1_013_567_488u64);
        let floor = headroom_floor(total, 512 * MB);
        assert_eq!(floor, total / 100 * 15);
        let size = headroom_file_size(floor, free, 512 * MB);
        assert_eq!(size % MB, 0);
        assert!((0..MB).contains(&(free + size - 2 * floor)));
    }

    #[test]
    fn small_hosts_double_the_two_chunk_floor() {
        // The notebook: 2303 MB of swap, 809 MB free when the trigger fired.
        let floor = headroom_floor(2303 * MB, 512 * MB);
        assert_eq!(floor, 1024 * MB);
        assert_eq!(headroom_file_size(floor, 809 * MB, 512 * MB), 1239 * MB);
    }

    #[test]
    fn headroom_file_is_never_below_one_chunk() {
        assert_eq!(headroom_file_size(1024 * MB, 2048 * MB, 512 * MB), 512 * MB);
        assert_eq!(headroom_file_size(0, 0, 512 * MB), 512 * MB);
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
