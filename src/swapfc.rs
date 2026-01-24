// SwapFC - Dynamic swap file management (btrfs only)
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::config::{Config, WORK_DIR};
use crate::helpers::{force_remove, makedirs, run_cmd_output};
use crate::meminfo::{get_free_ram_percent, get_free_swap_percent};
use crate::systemd::{gen_swap_unit, notify_ready, notify_status, systemctl};
use crate::{info, is_shutdown, warn};

#[derive(Error, Debug)]
pub enum SwapFcError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Helper error: {0}")]
    Helper(#[from] crate::helpers::HelperError),
    #[error("Systemd error: {0}")]
    Systemd(#[from] crate::systemd::SystemdError),
    #[error("Invalid swapfc_path")]
    InvalidPath,
    #[error("Unsupported filesystem (requires btrfs, ext4, or xfs)")]
    UnsupportedFs,
    #[error("Not enough space")]
    NoSpace,
}

pub type Result<T> = std::result::Result<T, SwapFcError>;

/// SwapFC configuration
#[derive(Debug)]
pub struct SwapFcConfig {
    pub path: PathBuf,
    pub chunk_size: u64,
    pub max_count: u32,
    pub min_count: u32,
    pub free_ram_perc: u8,
    pub free_swap_perc: u8,
    pub remove_free_swap_perc: u8,
    pub frequency: u64,
    pub priority: i32,
    pub force_use_loop: bool,
    pub directio: bool,
    pub use_btrfs_compression: bool,
    /// Use sparse files (thin provisioning) - opt-in, NOT default
    /// Pre-allocated files with fallocate are more stable under memory pressure
    pub use_sparse: bool,
}

impl SwapFcConfig {
    pub fn from_config(config: &Config) -> Result<Self> {
        let path = config.get("swapfc_path").unwrap_or("/swapfc/swapfile");
        let path = PathBuf::from(path.trim_end_matches('/'));

        // Parse chunk size (e.g., "512M")
        let chunk_size_str = config.get("swapfc_chunk_size").unwrap_or("512M");
        let chunk_size = parse_size(chunk_size_str)?;

        let max_count: u32 = config.get_as("swapfc_max_count").unwrap_or(32);
        let max_count = max_count.clamp(1, 32);

        let min_count: u32 = config.get_as("swapfc_min_count").unwrap_or(0);
        let frequency: u64 = config.get_as("swapfc_frequency").unwrap_or(1);
        let frequency = frequency.clamp(1, 86400);

        Ok(Self {
            path,
            chunk_size,
            max_count,
            min_count,
            free_ram_perc: config.get_as("swapfc_free_ram_perc").unwrap_or(35),
            free_swap_perc: config.get_as("swapfc_free_swap_perc").unwrap_or(25),
            remove_free_swap_perc: config.get_as("swapfc_remove_free_swap_perc").unwrap_or(55),
            frequency,
            priority: config.get_as("swapfc_priority").unwrap_or(50),
            force_use_loop: config.get_bool("swapfc_force_use_loop"),
            directio: config.get_bool("swapfc_directio"),
            use_btrfs_compression: config.get_bool("swapfc_use_btrfs_compression"),
            // Pre-allocated files (fallocate) are default for stability
            // Sparse files (thin provisioning) can be enabled with swapfc_use_sparse=1
            // but are less stable under memory pressure (can cause deadlocks)
            use_sparse: config.get_bool("swapfc_use_sparse"),
        })
    }
}

/// Parse size string like "512M", "1G", or "10%" (percentage of RAM) to bytes
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    
    // Check for percentage (e.g., "10%", "50%")
    if let Some(percent_str) = s.strip_suffix('%') {
        let percent: u64 = percent_str.parse().map_err(|_| SwapFcError::InvalidPath)?;
        if percent > 100 {
            return Err(SwapFcError::InvalidPath);
        }
        
        // Get total RAM and calculate percentage
        let ram_size = crate::meminfo::get_ram_size().map_err(|_| SwapFcError::InvalidPath)?;
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
            return s.parse().map_err(|_| SwapFcError::InvalidPath);
        }
    };

    num.parse::<u64>()
        .map(|n| n * multiplier)
        .map_err(|_| SwapFcError::InvalidPath)
}

/// SwapFC manager - supports btrfs, ext4, and xfs
pub struct SwapFc {
    config: SwapFcConfig,
    allocated: u32,
    block_size: u64,
    priority: i32,
    /// True if path is on btrfs (for subvolume/nodatacow handling)
    is_btrfs: bool,
}

impl SwapFc {
    /// Create new SwapFC manager
    pub fn new(config: &Config) -> Result<Self> {
        let swapfc_config = SwapFcConfig::from_config(config)?;

        notify_status("Monitoring memory status...");

        // Create parent directories
        makedirs(swapfc_config.path.parent().unwrap_or(Path::new("/")))?;

        // Detect filesystem type
        let fstype = get_path_fstype(&swapfc_config.path);
        let is_btrfs = fstype.as_deref() == Some("btrfs");
        
        // Verify supported filesystem
        match fstype.as_deref() {
            Some("btrfs") | Some("ext4") | Some("xfs") => {},
            Some(fs) => {
                warn!("swapFC: unsupported filesystem '{}', swap files may not work correctly", fs);
            },
            None => {
                warn!("swapFC: could not detect filesystem type");
            }
        }

        // Setup swap directory based on filesystem type
        if is_btrfs {
            // For btrfs: create subvolume with nodatacow for swap
            let is_subvolume = is_btrfs_subvolume(&swapfc_config.path);
            
            if !is_subvolume {
                if swapfc_config.path.exists() {
                    warn!("swapFC: path exists but not a subvolume, removing...");
                    if swapfc_config.path.is_dir() {
                        fs::remove_dir_all(&swapfc_config.path)?;
                    } else {
                        fs::remove_file(&swapfc_config.path)?;
                    }
                }

                let status = Command::new("btrfs")
                    .args(["subvolume", "create"])
                    .arg(&swapfc_config.path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?;

                if !status.success() {
                    return Err(SwapFcError::UnsupportedFs);
                }
                
                info!("swapFC: created btrfs subvolume at {:?}", swapfc_config.path);
            }
        } else {
            // For ext4/xfs: just create directory
            if !swapfc_config.path.exists() {
                fs::create_dir_all(&swapfc_config.path)?;
                info!("swapFC: created swap directory at {:?}", swapfc_config.path);
            }
        }

        let block_size = fs::metadata(&swapfc_config.path)?.blksize();
        makedirs(format!("{}/swapfc", WORK_DIR))?;

        let priority = swapfc_config.priority;

        Ok(Self {
            config: swapfc_config,
            allocated: 0,
            block_size,
            priority,
            is_btrfs,
        })
    }

    /// Create initial swap file (needed for zswap backing)
    pub fn create_initial_swap(&mut self) -> Result<()> {
        if self.allocated == 0 {
            self.create_swapfile()?;
        }
        Ok(())
    }

    /// Run the swap monitoring loop
    pub fn run(&mut self) -> Result<()> {
        notify_ready();

        if self.allocated == 0 {
            let memory_threshold = (crate::meminfo::get_ram_size().unwrap_or(0) as f64
                * (100 - self.config.free_ram_perc) as f64
                / (1024.0 * 1024.0 * 100.0)) as u64;
            info!("swapFC: on-demand swap at >{} MiB memory usage", memory_threshold);
        }

        loop {
            let poll_interval = self.get_adaptive_poll_interval();
            thread::sleep(Duration::from_secs(poll_interval));

            if is_shutdown() {
                break;
            }

            if self.allocated == 0 {
                let free_ram = get_free_ram_percent().unwrap_or(100);
                if free_ram < self.config.free_ram_perc {
                    info!("swapFC: RAM {}% < {}% - allocating first chunk", free_ram, self.config.free_ram_perc);
                    let _ = self.create_swapfile();
                }
                continue;
            }

            let free_swap = get_free_swap_percent().unwrap_or(100);

            // Allocate more swap chunks when free swap is low
            // With sparse files, this is fine - disk space is only used when zswap writes back
            if free_swap < self.config.free_swap_perc && self.allocated < self.config.max_count {
                info!("swapFC: swap {}% < {}% - allocating chunk #{}", free_swap, self.config.free_swap_perc, self.allocated + 1);
                let _ = self.create_swapfile();
                continue;
            }

            // Free swap chunks when swap usage is low
            if self.allocated > self.config.min_count.max(2) && free_swap > self.config.remove_free_swap_perc {
                info!("swapFC: swap {}% > {}% - freeing chunk #{}", free_swap, self.config.remove_free_swap_perc, self.allocated);
                let _ = self.destroy_swapfile();
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

    fn has_enough_space(&self) -> bool {
        if let Ok(stat) = nix::sys::statvfs::statvfs(&self.config.path) {
            let free_bytes = stat.blocks_available() as u64 * self.block_size;
            free_bytes >= self.config.chunk_size * 2
        } else {
            false
        }
    }

    fn create_swapfile(&mut self) -> Result<()> {
        if !self.has_enough_space() {
            warn!("swapFC: ENOSPC");
            notify_status("Not enough space");
            return Err(SwapFcError::NoSpace);
        }

        notify_status("Allocating swap file...");
        self.allocated += 1;

        let swapfile_path = self.config.path.join(self.allocated.to_string());

        // Remove if exists
        force_remove(&swapfile_path, false);

        // Create file with secure permissions (0600)
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .mode(0o600)
                .open(&swapfile_path)?;
        }

        // Determine allocation mode:
        // - Sparse: use truncate, only allocate disk space when data is written
        // - Preallocated: use fallocate, reserve all disk space upfront
        // Note: btrfs compression mode only makes sense on btrfs
        let use_compression = self.is_btrfs && self.config.use_btrfs_compression;
        let use_sparse = self.config.use_sparse || use_compression;
        
        // Sparse files require loop device for safe swap operation
        // Direct swap on sparse files can cause issues when kernel tries to write
        let use_loop = self.config.force_use_loop || use_sparse;

        if use_sparse {
            // Create sparse file (thin provisioning)
            // Disk space is only allocated when zswap/kernel actually writes data
            info!("swapFC: creating sparse file (thin provisioning)");
            Command::new("truncate")
                .args(["--size", &self.config.chunk_size.to_string()])
                .arg(&swapfile_path)
                .status()?;
            
            if self.is_btrfs && !use_compression {
                // Disable COW for btrfs when not using compression
                // This improves performance for swap workloads
                let _ = Command::new("chattr").args(["+C"]).arg(&swapfile_path).status();
            }
            // When using compression, do NOT set +C - we want compression!
        } else {
            // Preallocated mode: reserve all disk space upfront
            if self.is_btrfs {
                // Disable COW for btrfs
                let _ = Command::new("chattr").args(["+C"]).arg(&swapfile_path).status();
            }

            // Allocate space with fallocate
            info!("swapFC: creating preallocated file");
            Command::new("fallocate")
                .args(["-l", &self.config.chunk_size.to_string()])
                .arg(&swapfile_path)
                .status()?;
        }

        let (swapfile, loop_device) = if use_loop {
            // Create loop device
            // For sparse files, disable direct-io to allow proper thin provisioning
            let directio = if self.config.directio && !use_sparse { "on" } else { "off" };
            let loop_dev = run_cmd_output(&[
                "losetup", "-f", "--show",
                &format!("--direct-io={}", directio),
                &swapfile_path.to_string_lossy(),
            ])?;
            let loop_dev = loop_dev.trim().to_string();
            (loop_dev.clone(), Some(loop_dev))
        } else {
            (swapfile_path.to_string_lossy().to_string(), None)
        };

        // mkswap
        Command::new("mkswap")
            .args(["-L", &format!("SWAP_btrfs_{}", self.allocated)])
            .arg(&swapfile)
            .stdout(Stdio::null())
            .status()?;

        // Generate and start swap unit
        // Use discard=pages for compressed mode to release space when pages are freed
        let discard_option = if use_compression { "pages" } else { "discard" };
        let unit_name = gen_swap_unit(
            Path::new(&swapfile),
            Some(self.priority),
            Some(discard_option),
            &format!("swapfc_{}", self.allocated),
        )?;

        // Store loop device info for cleanup
        if let Some(ref loop_dev) = loop_device {
            let loop_info_path = format!("{}/swapfc/loop_{}", WORK_DIR, self.allocated);
            let _ = fs::write(&loop_info_path, format!("{}\n{}", loop_dev, swapfile_path.display()));
        }

        self.priority -= 1;

        systemctl("daemon-reload", "")?;
        systemctl("start", &unit_name)?;

        notify_status("Monitoring memory status...");
        Ok(())
    }

    fn destroy_swapfile(&mut self) -> Result<()> {
        notify_status("Deallocating swap file...");

        let tag = format!("swapfc_{}", self.allocated);

        // Check if we have loop device info for this swap
        let loop_info_path = format!("{}/swapfc/loop_{}", WORK_DIR, self.allocated);
        let loop_info = fs::read_to_string(&loop_info_path).ok();

        for unit_path in crate::helpers::find_swap_units() {
            if let Ok(content) = crate::helpers::read_file(&unit_path) {
                if content.contains(&tag) {
                    if let Some(dev) = crate::helpers::get_what_from_swap_unit(&unit_path) {
                        let unit_name = Path::new(&unit_path)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();

                        if systemctl("stop", &unit_name).is_err() {
                            let _ = crate::systemd::swapoff(&dev);
                        }

                        force_remove(&unit_path, true);

                        // Clean up loop device and backing file if applicable
                        if let Some(ref info) = loop_info {
                            let lines: Vec<&str> = info.lines().collect();
                            if lines.len() >= 2 {
                                let loop_dev = lines[0];
                                let backing_file = lines[1];
                                
                                // Detach loop device
                                let _ = Command::new("losetup")
                                    .args(["-d", loop_dev])
                                    .status();
                                
                                // Remove backing file
                                force_remove(backing_file, false);
                            }
                            // Remove loop info file
                            force_remove(&loop_info_path, false);
                        } else if Path::new(&dev).is_file() {
                            force_remove(&dev, false);
                        }
                    }
                    break;
                }
            }
        }

        self.allocated -= 1;
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

/// Get the filesystem type of a given path
fn get_path_fstype(path: &Path) -> Option<String> {
    // Use parent if path doesn't exist
    let check_path = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|p| p.exists() && *p != Path::new("/"))
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"))
    };

    let output = Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "--target"])
        .arg(&check_path)
        .stdout(Stdio::piped())
        .output()
        .ok()?;

    let fstype = String::from_utf8_lossy(&output.stdout).trim().to_lowercase();
    if fstype.is_empty() {
        // Fallback to root filesystem
        if check_path != PathBuf::from("/") {
            Command::new("findmnt")
                .args(["-n", "-o", "FSTYPE", "/"])
                .stdout(Stdio::piped())
                .output()
                .ok()
                .and_then(|o| {
                    let fs = String::from_utf8_lossy(&o.stdout).trim().to_lowercase();
                    if fs.is_empty() { None } else { Some(fs) }
                })
        } else {
            None
        }
    } else {
        Some(fstype)
    }
}
