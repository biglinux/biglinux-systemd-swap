// SwapFC - Dynamic swap file management (btrfs only)
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::autoconfig::{get_swap_partition_stats, StorageType};
use crate::config::{Config, WORK_DIR};
use crate::helpers::{force_remove, get_fstype, makedirs, run_cmd_output};
use crate::meminfo::{get_free_ram_percent, get_free_swap_percent};
use crate::systemd::{gen_swap_unit, notify_ready, notify_status, swapoff, systemctl};
use crate::{debug, info, is_shutdown, warn};

/// Discard policy for swap on SSDs/NVMe
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardPolicy {
    /// No discard (for HDDs or if causing issues)
    None,
    /// Discard only on swapoff (recommended for most SSDs)
    Once,
    /// Continuous discard on each freed page (may impact performance)
    Pages,
    /// Both once + pages
    Both,
    /// Auto-detect based on storage type
    Auto,
}

impl DiscardPolicy {
    /// Parse from config string
    pub fn parse_str(s: &str) -> Self {
        match s.to_lowercase().trim() {
            "none" | "off" | "0" => DiscardPolicy::None,
            "once" => DiscardPolicy::Once,
            "pages" => DiscardPolicy::Pages,
            "both" | "all" => DiscardPolicy::Both,
            _ => DiscardPolicy::Auto, // Default to auto
        }
    }

    /// Convert to systemd swap unit Options string
    pub fn to_options_string(&self, storage_type: &StorageType) -> Option<&'static str> {
        match self {
            DiscardPolicy::None => None,
            DiscardPolicy::Once => Some("discard=once"),
            DiscardPolicy::Pages => Some("discard=pages"),
            DiscardPolicy::Both => Some("discard"),
            DiscardPolicy::Auto => {
                // Auto: enable discard for SSDs/NVMe, disable for HDDs
                match storage_type {
                    StorageType::NVMe | StorageType::SSD => Some("discard=once"),
                    StorageType::EMMC => Some("discard=once"),
                    _ => None, // HDD, SD, Unknown - no discard
                }
            }
        }
    }
}

/// Calculate swap priority based on storage type
/// Higher priority = used first by the kernel
pub fn calculate_auto_priority(storage_type: &StorageType) -> i32 {
    match storage_type {
        StorageType::Tmpfs => 150, // RAM-based, highest priority
        StorageType::NVMe => 100,  // Fastest physical storage
        StorageType::SSD => 75,    // Fast
        StorageType::EMMC => 50,   // Moderate
        StorageType::SD => 25,     // Slow, avoid wearing
        StorageType::HDD => 10,    // Slowest
        StorageType::Unknown => 0, // Unknown, lowest priority
    }
}

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
    /// With progressive scaling, this is the starting size that doubles every N files
    pub chunk_size: u64,
    pub max_count: u32,
    pub min_count: u32,
    pub free_ram_perc: u8,
    pub free_swap_perc: u8,
    pub remove_free_swap_perc: u8,
    pub frequency: u64,
    /// Priority for swap files (-1 = auto-calculate based on storage type)
    pub priority: i32,
    pub force_use_loop: bool,
    /// Force disable direct I/O (direct I/O is now ON by default for preallocated files)
    pub directio_disable: bool,
    /// Enable progressive chunk scaling (doubles every 4 files)
    /// This allows efficient use of the 28-32 file limit:
    /// - Files 1-4: base_size (e.g., 512MB)
    /// - Files 5-8: 2x base_size (1GB)
    /// - Files 9-12: 4x base_size (2GB)
    /// - Files 13-16: 8x base_size (4GB)
    /// - etc.
    pub progressive_scaling: bool,
    /// How many files to create before doubling the size
    pub scaling_step: u32,
    /// Maximum chunk size (cap for progressive scaling)
    pub max_chunk_size: u64,
    /// Individual file usage threshold for removal consideration (default: 30%)
    pub shrink_threshold: u8,
    /// Safe headroom percentage to maintain in other files after migration (default: 40%)
    pub safe_headroom: u8,
    /// Discard policy for SSDs (auto, none, once, pages, both)
    pub discard_policy: DiscardPolicy,
    /// Whether to consider swap partitions in expansion decisions
    /// If true and partitions exist, only create swapfiles when partition usage >= partition_threshold
    pub use_partitions: bool,
    /// Threshold for partition usage before creating swapfiles (default: 90%)
    pub partition_threshold: u8,
}

impl SwapFileConfig {
    /// Create config from parsed Config file
    /// Supports both swapfile_* and legacy swapfc_* config keys
    pub fn from_config(config: &Config) -> Result<Self> {
        // Helper: try swapfile_* key first, fall back to swapfc_* for backward compat
        let get_compat = |new_key: &str, old_key: &str, default: &str| -> String {
            config
                .get(new_key)
                .or_else(|_| config.get(old_key))
                .unwrap_or(default)
                .to_string()
        };
        let get_compat_as = |new_key: &str, old_key: &str, default: u32| -> u32 {
            config
                .get_as(new_key)
                .or_else(|_| config.get_as(old_key))
                .unwrap_or(default)
        };
        let get_compat_bool = |new_key: &str, old_key: &str| -> bool {
            if config.has_explicit(new_key) {
                config.get_bool(new_key)
            } else {
                config.get_bool(old_key)
            }
        };

        let path = get_compat("swapfile_path", "swapfc_path", "/swapfile");
        let path = PathBuf::from(path.trim_end_matches('/'));

        let chunk_size_str = get_compat("swapfile_chunk_size", "swapfc_chunk_size", "512M");
        let chunk_size = parse_size(&chunk_size_str)?;

        let max_count: u32 = get_compat_as("swapfile_max_count", "swapfc_max_count", 28);
        let max_count = max_count.clamp(1, 28);

        let min_count: u32 = get_compat_as("swapfile_min_count", "swapfc_min_count", 1);
        let frequency: u64 = get_compat_as("swapfile_frequency", "swapfc_frequency", 1) as u64;
        let frequency = frequency.clamp(1, 86400);

        let progressive_scaling =
            !get_compat_bool("swapfile_progressive_disable", "swapfc_progressive_disable");
        let scaling_step: u32 = get_compat_as("swapfile_scaling_step", "swapfc_scaling_step", 4);
        let scaling_step = scaling_step.clamp(2, 8);

        let max_chunk_size_str =
            get_compat("swapfile_max_chunk_size", "swapfc_max_chunk_size", "32G");
        let max_chunk_size = parse_size(&max_chunk_size_str).unwrap_or(32 * 1024 * 1024 * 1024);

        let shrink_threshold: u8 =
            get_compat_as("swapfile_shrink_threshold", "swapfc_shrink_threshold", 30) as u8;
        let shrink_threshold = shrink_threshold.clamp(10, 50);

        let safe_headroom: u8 =
            get_compat_as("swapfile_safe_headroom", "swapfc_safe_headroom", 40) as u8;
        let safe_headroom = safe_headroom.clamp(20, 60);

        let discard_str = get_compat("swapfile_discard", "swapfc_discard", "auto");
        let discard_policy = DiscardPolicy::parse_str(&discard_str);

        Ok(Self {
            path,
            chunk_size,
            max_count,
            min_count,
            free_ram_perc: get_compat_as("swapfile_free_ram_perc", "swapfc_free_ram_perc", 20)
                as u8,
            free_swap_perc: get_compat_as("swapfile_free_swap_perc", "swapfc_free_swap_perc", 40)
                as u8,
            remove_free_swap_perc: get_compat_as(
                "swapfile_remove_free_swap_perc",
                "swapfc_remove_free_swap_perc",
                70,
            ) as u8,
            frequency,
            priority: config
                .get_as("swapfile_priority")
                .or_else(|_| config.get_as("swapfc_priority"))
                .unwrap_or(-1),
            force_use_loop: get_compat_bool("swapfile_force_use_loop", "swapfc_force_use_loop"),
            directio_disable: get_compat_bool(
                "swapfile_directio_disable",
                "swapfc_directio_disable",
            ),
            progressive_scaling,
            scaling_step,
            max_chunk_size,
            shrink_threshold,
            safe_headroom,
            discard_policy,
            use_partitions: get_compat_bool("swapfile_use_partitions", "swapfc_use_partitions"),
            partition_threshold: get_compat_as(
                "swapfile_partition_threshold",
                "swapfc_partition_threshold",
                90,
            ) as u8,
        })
    }
}

/// Parse size string like "512M", "1G", or "10%" (percentage of RAM) to bytes
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();

    // Check for percentage (e.g., "10%", "50%")
    if let Some(percent_str) = s.strip_suffix('%') {
        let percent: u64 = percent_str
            .parse()
            .map_err(|_| SwapFileError::InvalidPath)?;
        if percent > 100 {
            return Err(SwapFileError::InvalidPath);
        }

        // Get total RAM and calculate percentage
        let ram_size = crate::meminfo::get_ram_size().map_err(|_| SwapFileError::InvalidPath)?;
        return Ok(ram_size * percent / 100);
    }

    let (num, suffix) = s.split_at(s.len().saturating_sub(1));

    let multiplier = match suffix.to_uppercase().as_str() {
        "K" => 1024u64,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024 * 1024 * 1024 * 1024,
        _ => {
            // No suffix, try parsing whole string as bytes
            return s.parse().map_err(|_| SwapFileError::InvalidPath);
        }
    };

    num.parse::<u64>()
        .map(|n| n * multiplier)
        .map_err(|_| SwapFileError::InvalidPath)
}

/// SwapFC manager - supports btrfs, ext4, and xfs
pub struct SwapFile {
    config: SwapFileConfig,
    allocated: u32,
    priority: i32,
    /// True if path is on btrfs (for subvolume/nodatacow handling)
    is_btrfs: bool,
    /// Track the size of each allocated file (for proper cleanup and stats)
    file_sizes: Vec<u64>,
    /// Detected storage type for optimization decisions
    storage_type: StorageType,
}

impl SwapFile {
    /// Create new SwapFC manager
    pub fn new(config: &Config) -> Result<Self> {
        let swapfile_config = SwapFileConfig::from_config(config)?;

        if swapfile_config.progressive_scaling {
            info!(
                "swapFC: progressive scaling enabled (step={})",
                swapfile_config.scaling_step
            );
            info!(
                "swapFC: base={}MB, max={}GB",
                swapfile_config.chunk_size / (1024 * 1024),
                swapfile_config.max_chunk_size / (1024 * 1024 * 1024)
            );
        }

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

                    // Set nodatacow attribute if possible
                    let _ = Command::new("chattr")
                        .args(["+C"])
                        .arg(&swapfile_config.path)
                        .status();

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

        let block_size = fs::metadata(&swapfile_config.path)?.blksize();
        let _ = block_size; // Used for mkswap alignment, not needed in struct
        makedirs(format!("{}/swapfile", WORK_DIR))?;

        // Detect storage type for optimizations
        let storage_type = StorageType::detect(&swapfile_config.path.to_string_lossy());
        info!("swapFC: detected storage type: {:?}", storage_type);

        // Calculate priority: auto-detect if -1, otherwise use configured value
        let priority = if swapfile_config.priority < 0 {
            let auto_priority = calculate_auto_priority(&storage_type);
            info!(
                "swapFC: auto-calculated priority {} for {:?}",
                auto_priority, storage_type
            );
            auto_priority
        } else {
            swapfile_config.priority
        };

        // Log discard policy
        if let Some(discard_opt) = swapfile_config
            .discard_policy
            .to_options_string(&storage_type)
        {
            info!(
                "swapFC: discard policy: {:?} -> {}",
                swapfile_config.discard_policy, discard_opt
            );
        } else {
            info!("swapFC: discard disabled for {:?}", storage_type);
        }

        Ok(Self {
            config: swapfile_config,
            allocated: 0,
            priority,
            is_btrfs,
            file_sizes: Vec::new(),
            storage_type,
        })
    }

    /// Calculate the chunk size for a given file number using progressive scaling
    ///
    /// With scaling_step=4 and base_size=512MB:
    /// - Files 1-4: 512MB each (tier 0)
    /// - Files 5-8: 1GB each (tier 1)
    /// - Files 9-12: 2GB each (tier 2)
    /// - Files 13-16: 4GB each (tier 3)
    /// - Files 17-20: 8GB each (tier 4)
    /// - Files 21-24: 16GB each (tier 5)
    /// - Files 25-28: 32GB each (tier 6)
    ///
    /// This allows up to 254GB of swap with only 28 files!
    fn get_chunk_size_for_file(&self, file_num: u32) -> u64 {
        if !self.config.progressive_scaling {
            return self.config.chunk_size;
        }

        // Calculate tier: (file_num - 1) / scaling_step
        // file_num is 1-based, so file 1-4 = tier 0, 5-8 = tier 1, etc.
        let tier = (file_num.saturating_sub(1)) / self.config.scaling_step;

        // Size = base_size * 2^tier, capped at max_chunk_size
        let size = self.config.chunk_size.saturating_mul(1u64 << tier);

        size.min(self.config.max_chunk_size)
    }

    /// Calculate dynamic expansion threshold based on number of allocated files
    /// When there are few files, expand earlier to avoid pressure
    /// When there are many, wait longer before creating new ones
    fn get_expand_threshold(&self) -> u8 {
        match self.allocated {
            0 | 1 => 50, // 1 file → expand when usage > 50%
            2 => 60,     // 2 files → expand when usage > 60%
            3 => 70,     // 3 files → expand when usage > 70%
            _ => 80,     // 4+ files → expand when usage > 80%
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
            let is_our_file = path.starts_with(&self.config.path)
                || (path.starts_with("/dev/loop") && self.is_our_loop_device(&path));

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
        // Check loop info files in our work directory
        for i in 1..=self.allocated {
            let loop_info_path = format!("{}/swapfile/loop_{}", WORK_DIR, i);
            if let Ok(content) = fs::read_to_string(&loop_info_path) {
                let lines: Vec<&str> = content.lines().collect();
                if !lines.is_empty() && lines[0] == loop_path.to_string_lossy() {
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
        let candidates: Vec<&SwapFileInfo> = files
            .iter()
            .filter(|f| f.is_nearly_empty(self.config.shrink_threshold))
            .collect();

        if candidates.is_empty() {
            return None; // No file is empty enough
        }

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
        let is_loop = path.starts_with("/dev/loop");
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
        for i in 1..=self.allocated {
            let loop_info_path = format!("{}/swapfile/loop_{}", WORK_DIR, i);
            if let Ok(content) = fs::read_to_string(&loop_info_path) {
                let lines: Vec<&str> = content.lines().collect();
                if lines.len() >= 2 && lines[0] == loop_path.to_string_lossy() {
                    return Some(PathBuf::from(lines[1]));
                }
            }
        }
        None
    }

    /// Create initial swap file (needed for zswap backing)
    pub fn create_initial_swap(&mut self) -> Result<()> {
        if self.allocated == 0 {
            self.create_swapfile()?;
        }
        Ok(())
    }

    /// Run the swap monitoring loop with intelligent expansion/contraction
    /// Uses THREE triggers for expansion (any one is sufficient):
    ///   1. RAM pressure: free RAM < free_ram_perc (most critical, prevents OOM)
    ///   2. Swap pressure: free swap < free_swap_perc (proactive expansion)
    ///   3. File usage: internal swap file usage > dynamic threshold (original logic)
    /// Uses TWO triggers for contraction:
    ///   1. Swap abundance: free swap > remove_free_swap_perc
    ///   2. File emptiness: individual file usage < shrink_threshold (original logic)
    pub fn run(&mut self) -> Result<()> {
        notify_ready();

        // Ensure minimum files are created at startup
        while self.allocated < self.config.min_count {
            info!(
                "swapFC: creating initial file #{} (min_count={})",
                self.allocated + 1,
                self.config.min_count
            );
            self.create_swapfile()?;
        }

        loop {
            let poll_interval = self.get_adaptive_poll_interval();
            thread::sleep(Duration::from_secs(poll_interval));

            if is_shutdown() {
                break;
            }

            // Read system-wide pressure indicators
            let free_ram = get_free_ram_percent().unwrap_or(100);
            let free_swap = get_free_swap_percent().unwrap_or(100);

            // Get individual file statistics from /proc/swaps
            let swap_files = self.get_swapfiles_info();

            // Calculate global statistics for swapfiles
            let total_size: u64 = swap_files.iter().map(|f| f.size_bytes).sum();
            let total_used: u64 = swap_files.iter().map(|f| f.used_bytes).sum();
            let global_usage = if total_size > 0 {
                ((total_used * 100) / total_size) as u8
            } else {
                0
            };

            // Check swap partition usage if configured to use partitions
            let should_skip_expansion = if self.config.use_partitions {
                let (part_total, part_used) = get_swap_partition_stats();
                if part_total > 0 {
                    let part_usage = ((part_used * 100) / part_total) as u8;
                    if part_usage < self.config.partition_threshold {
                        debug!("swapFC: partitions at {}% (< {}% threshold), deferring swapfile creation", 
                            part_usage, self.config.partition_threshold);
                        true // Skip expansion, partitions have capacity
                    } else {
                        info!(
                            "swapFC: partitions at {}% (>= {}% threshold), can create swapfiles",
                            part_usage, self.config.partition_threshold
                        );
                        false // Partitions full, can expand with swapfiles
                    }
                } else {
                    false // No partitions, proceed normally
                }
            } else {
                false // Not using partitions, proceed normally
            };

            if !should_skip_expansion && self.allocated < self.config.max_count {
                // EXPANSION TRIGGER 1 (highest priority): RAM pressure
                // If free RAM is below the configured threshold, expand IMMEDIATELY
                // This is the critical trigger that prevents OOM kills
                if free_ram < self.config.free_ram_perc {
                    info!(
                        "swapFC: RAM pressure! free_ram={}% < {}% - expanding urgently",
                        free_ram, self.config.free_ram_perc
                    );
                    let _ = self.create_swapfile();
                    continue;
                }

                // EXPANSION TRIGGER 2: Swap space pressure
                // If total swap free is below the configured threshold, expand proactively
                if free_swap < self.config.free_swap_perc {
                    info!(
                        "swapFC: swap pressure! free_swap={}% < {}% - expanding",
                        free_swap, self.config.free_swap_perc
                    );
                    let _ = self.create_swapfile();
                    continue;
                }

                // EXPANSION TRIGGER 3 (original): internal file usage threshold
                let expand_threshold = self.get_expand_threshold();
                if global_usage >= expand_threshold {
                    info!("swapFC: global usage {}% >= {}% (dynamic threshold for {} files) - expanding", 
                        global_usage, expand_threshold, self.allocated);
                    let _ = self.create_swapfile();
                    continue;
                }
            }

            // CONTRACTION DECISION: check if swap is abundant enough to remove files
            if self.allocated > self.config.min_count {
                // Only attempt contraction when swap is abundant
                if free_swap > self.config.remove_free_swap_perc {
                    if let Some(candidate) = self.find_safe_removal_candidate(&swap_files) {
                        info!(
                            "swapFC: free_swap={}% > {}%, removing {} (usage: {}%)",
                            free_swap,
                            self.config.remove_free_swap_perc,
                            candidate.path.display(),
                            candidate.usage_percent()
                        );
                        let path = candidate.path.clone();
                        let _ = self.destroy_swapfile_by_path(&path);
                    }
                } else if let Some(candidate) = self.find_safe_removal_candidate(&swap_files) {
                    // Also allow removal of nearly-empty files even when swap isn't abundant
                    if candidate.usage_percent() == 0 {
                        info!(
                            "swapFC: removing empty file {} (usage: 0%)",
                            candidate.path.display()
                        );
                        let path = candidate.path.clone();
                        let _ = self.destroy_swapfile_by_path(&path);
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
        if let Ok(stat) = nix::sys::statvfs::statvfs(&self.config.path) {
            let free_bytes = stat.blocks_available() * stat.block_size();
            // Need at least 2x the required size (safety margin)
            free_bytes >= required_size * 2
        } else {
            false
        }
    }

    fn create_swapfile(&mut self) -> Result<()> {
        // Calculate size for the next file using progressive scaling
        let next_file_num = self.allocated + 1;
        let chunk_size = self.get_chunk_size_for_file(next_file_num);

        if !self.has_enough_space(chunk_size) {
            warn!("swapFC: ENOSPC (need {}MB)", chunk_size / (1024 * 1024));
            notify_status("Not enough space");
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

        // Always disable COW on btrfs for better swap performance
        if self.is_btrfs {
            let _ = Command::new("chattr")
                .args(["+C"])
                .arg(&swapfile_path)
                .status();
        }

        // Always use fallocate to preallocate space
        // This guarantees disk space is reserved and prevents system freezes
        info!(
            "swapFC: creating preallocated file #{} ({}MB)",
            self.allocated,
            chunk_size / (1024 * 1024)
        );
        let status = Command::new("fallocate")
            .args(["-l", &chunk_size.to_string()])
            .arg(&swapfile_path)
            .status()?;
        if !status.success() {
            force_remove(&swapfile_path, false);
            self.allocated -= 1;
            self.file_sizes.pop();
            return Err(SwapFileError::NoSpace);
        }

        // Loop device only if explicitly requested
        let use_loop = self.config.force_use_loop;

        let (swapfile, loop_device) = if use_loop {
            // Create loop device with direct I/O (unless disabled)
            let directio = if self.config.directio_disable {
                "off"
            } else {
                "on"
            };
            let loop_dev = run_cmd_output(&[
                "losetup",
                "-f",
                "--show",
                &format!("--direct-io={}", directio),
                &swapfile_path.to_string_lossy(),
            ])?;
            let loop_dev = loop_dev.trim().to_string();
            (loop_dev.clone(), Some(loop_dev))
        } else {
            (swapfile_path.to_string_lossy().to_string(), None)
        };

        // mkswap
        let status = Command::new("mkswap")
            .args(["-L", &format!("SWAP_btrfs_{}", self.allocated)])
            .arg(&swapfile)
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            force_remove(&swapfile_path, false);
            self.allocated -= 1;
            self.file_sizes.pop();
            return Err(SwapFileError::Io(std::io::Error::other("mkswap failed")));
        }

        // Generate and start swap unit with discard policy based on storage type
        let discard_options = self
            .config
            .discard_policy
            .to_options_string(&self.storage_type);
        let unit_name = gen_swap_unit(
            Path::new(&swapfile),
            Some(self.priority),
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

        self.priority -= 1;

        systemctl("daemon-reload", "")?;
        systemctl("start", &unit_name)?;

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
