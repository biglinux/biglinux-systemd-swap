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
    #[error("Not btrfs filesystem")]
    NotBtrfs,
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
        })
    }
}

/// Parse size string like "512M" or "1G" to bytes
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, suffix) = s.split_at(s.len().saturating_sub(1));
    
    let multiplier = match suffix.to_uppercase().as_str() {
        "K" => 1024u64,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024 * 1024 * 1024 * 1024,
        _ => {
            // No suffix, try parsing whole string
            return s.parse().map_err(|_| SwapFcError::InvalidPath);
        }
    };

    num.parse::<u64>()
        .map(|n| n * multiplier)
        .map_err(|_| SwapFcError::InvalidPath)
}

/// SwapFC manager (btrfs only)
pub struct SwapFc {
    config: SwapFcConfig,
    allocated: u32,
    block_size: u64,
    priority: i32,
}

impl SwapFc {
    /// Create new SwapFC manager
    pub fn new(config: &Config) -> Result<Self> {
        let swapfc_config = SwapFcConfig::from_config(config)?;

        notify_status("Monitoring memory status...");

        // Create parent directories
        makedirs(swapfc_config.path.parent().unwrap_or(Path::new("/")))?;

        // Verify btrfs and setup subvolume
        let is_subvolume = is_btrfs_subvolume(&swapfc_config.path);
        
        if !is_subvolume {
            // Create btrfs subvolume
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
                return Err(SwapFcError::NotBtrfs);
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
        })
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

            if free_swap < self.config.free_swap_perc && self.allocated < self.config.max_count {
                info!("swapFC: swap {}% < {}% - allocating chunk #{}", free_swap, self.config.free_swap_perc, self.allocated + 1);
                let _ = self.create_swapfile();
                continue;
            }

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

        // Disable COW for btrfs
        let _ = Command::new("chattr").args(["+C"]).arg(&swapfile_path).status();

        // Allocate space
        Command::new("fallocate")
            .args(["-l", &self.config.chunk_size.to_string()])
            .arg(&swapfile_path)
            .status()?;

        let swapfile = if self.config.force_use_loop {
            let directio = if self.config.directio { "on" } else { "off" };
            let loop_dev = run_cmd_output(&[
                "losetup", "-f", "--show",
                &format!("--direct-io={}", directio),
                &swapfile_path.to_string_lossy(),
            ])?;
            fs::remove_file(&swapfile_path)?;
            loop_dev
        } else {
            swapfile_path.to_string_lossy().to_string()
        };

        // mkswap
        Command::new("mkswap")
            .args(["-L", &format!("SWAP_btrfs_{}", self.allocated)])
            .arg(&swapfile)
            .stdout(Stdio::null())
            .status()?;

        // Generate and start swap unit
        let unit_name = gen_swap_unit(
            Path::new(&swapfile),
            Some(self.priority),
            Some("discard"),
            &format!("swapfc_{}", self.allocated),
        )?;

        self.priority -= 1;

        systemctl("daemon-reload", "")?;
        systemctl("start", &unit_name)?;

        notify_status("Monitoring memory status...");
        Ok(())
    }

    fn destroy_swapfile(&mut self) -> Result<()> {
        notify_status("Deallocating swap file...");

        let tag = format!("swapfc_{}", self.allocated);

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

                        if Path::new(&dev).is_file() {
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
