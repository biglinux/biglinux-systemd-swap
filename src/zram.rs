// Zram configuration for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::config::{Config, WORK_DIR};
use crate::helpers::{makedirs, read_file};
use crate::systemd::{gen_swap_unit, systemctl};
use crate::{error, info, warn};

const ZRAM_MODULE: &str = "/sys/module/zram";
const ZRAM_HOT_ADD: &str = "/sys/class/zram-control/hot_add";

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
}

pub type Result<T> = std::result::Result<T, ZramError>;

/// Check if zram is available
pub fn is_available() -> bool {
    Path::new(ZRAM_MODULE).is_dir()
}

/// Parse zram size from config string
/// Supports:
/// - Absolute bytes: 1073741824
/// - With suffix: 1G, 512M, 256K
/// - Percentage of RAM: 50%, 100%
fn parse_zram_size(size_str: &str) -> Result<u64> {
    let s = size_str.trim();
    
    // Handle percentage (e.g., "50%", "100%")
    if let Some(percent_str) = s.strip_suffix('%') {
        let percent: u64 = percent_str.parse()
            .map_err(|_| ZramError::ZramctlFailed(format!("Invalid percentage: {}", s)))?;
        let ram_size = crate::meminfo::get_ram_size()
            .map_err(|e| ZramError::ZramctlFailed(format!("Failed to get RAM size: {}", e)))?;
        return Ok(ram_size * percent / 100);
    }
    
    // Handle size with suffix (e.g., "1G", "512M")
    let (num_part, multiplier) = if s.len() > 1 {
        let last_char = s.chars().last().unwrap().to_ascii_uppercase();
        match last_char {
            'K' => (&s[..s.len()-1], 1024u64),
            'M' => (&s[..s.len()-1], 1024 * 1024),
            'G' => (&s[..s.len()-1], 1024 * 1024 * 1024),
            'T' => (&s[..s.len()-1], 1024 * 1024 * 1024 * 1024),
            _ => (s, 1u64),  // No suffix, treat as bytes
        }
    } else {
        (s, 1u64)
    };
    
    num_part.parse::<u64>()
        .map(|n| n * multiplier)
        .map_err(|_| ZramError::ZramctlFailed(format!("Invalid size: {}", s)))
}

/// Create backing file and loop device for zram writeback
fn create_writeback_backing(config: &Config) -> Result<String> {
    // Use same path as swapfc for consistency
    let swapfc_path = config.get("swapfc_path").unwrap_or("/swapfc/swapfile");
    let parent = Path::new(swapfc_path).parent().unwrap_or(Path::new("/swapfc"));
    
    makedirs(parent)?;
    
    let backing_file = parent.join("zram_writeback");
    
    // Remove if exists
    if backing_file.exists() {
        std::fs::remove_file(&backing_file)?;
    }
    
    // Create sparse file (1GB default) - will grow as needed
    let size = config.get("zram_writeback_size").unwrap_or("1G");
    let size_bytes = parse_zram_size(size)?;
    
    info!("Zram: creating writeback backing file: {} ({} bytes)", backing_file.display(), size_bytes);
    
    // Create file with correct permissions
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o600)
            .open(&backing_file)?;
    }
    
    // Use truncate for sparse file
    Command::new("truncate")
        .args(["--size", &size_bytes.to_string()])
        .arg(&backing_file)
        .status()?;
    
    // Create loop device
    let output = Command::new("losetup")
        .args(["-f", "--show"])
        .arg(&backing_file)
        .stdout(Stdio::piped())
        .output()?;
    
    if !output.status.success() {
        return Err(ZramError::ZramctlFailed("losetup failed".to_string()));
    }
    
    let loop_dev = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!("Zram: created loop device {} for writeback", loop_dev);
    
    // Save backing file info for cleanup
    let info_path = format!("{}/zram/writeback_file", WORK_DIR);
    let _ = std::fs::write(&info_path, backing_file.to_string_lossy().as_ref());
    
    Ok(loop_dev)
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
    let zram_size = parse_zram_size(config.get("zram_size").unwrap_or("0"))?;
    let zram_alg = config.get("zram_alg").unwrap_or("lzo");
    let zram_prio: i32 = config.get_as("zram_prio").unwrap_or(32767);
    let zram_writeback = config.get_bool("zram_writeback");
    let zram_writeback_dev = config.get("zram_writeback_dev").unwrap_or("");

    if zram_size == 0 {
        warn!("Zram: size is 0, skipping");
        return Ok(());
    }

    info!("Zram: size = {} bytes ({} MiB)", zram_size, zram_size / (1024 * 1024));

    let mut writeback_loop: Option<String> = None;
    let zram_dev: String;
    
    if zram_writeback {
        // When writeback is enabled, we must configure device manually via sysfs
        // Order: hot_add → backing_dev → comp_algorithm → disksize
        info!("Zram: writeback mode - configuring via sysfs");
        
        // Get backing device first
        let backing_device = if zram_writeback_dev.is_empty() {
            info!("Zram: creating auto backing device for writeback");
            match create_writeback_backing(config) {
                Ok(loop_dev) => {
                    writeback_loop = Some(loop_dev.clone());
                    loop_dev
                }
                Err(e) => {
                    warn!("Zram: failed to create backing device: {}, continuing without writeback", e);
                    String::new()
                }
            }
        } else {
            zram_writeback_dev.to_string()
        };
        
        // Add new zram device via hot_add
        if !Path::new(ZRAM_HOT_ADD).exists() {
            error!("Zram: kernel doesn't support hot adding devices");
            return Err(ZramError::NoFreeDevice);
        }
        
        let new_id = read_file(ZRAM_HOT_ADD)?.trim().to_string();
        zram_dev = format!("/dev/zram{}", new_id);
        let zram_sysfs = format!("/sys/block/zram{}", new_id);
        
        info!("Zram: created new device: {}", zram_dev);
        
        // Configure backing device FIRST (before disksize!)
        if !backing_device.is_empty() {
            let backing_path = format!("{}/backing_dev", zram_sysfs);
            if Path::new(&backing_path).exists() {
                info!("Zram: configuring backing device: {}", backing_device);
                if let Err(e) = std::fs::write(&backing_path, &backing_device) {
                    warn!("Zram: failed to set backing_dev: {}", e);
                }
            } else {
                warn!("Zram: writeback not supported (CONFIG_ZRAM_WRITEBACK not enabled)");
            }
        }
        
        // Set compression algorithm
        let comp_path = format!("{}/comp_algorithm", zram_sysfs);
        if let Err(e) = std::fs::write(&comp_path, zram_alg) {
            warn!("Zram: failed to set comp_algorithm: {}", e);
        }
        
        // Set disksize LAST
        let disksize_path = format!("{}/disksize", zram_sysfs);
        if let Err(e) = std::fs::write(&disksize_path, zram_size.to_string()) {
            error!("Zram: failed to set disksize: {}", e);
            return Err(ZramError::ZramctlFailed("Failed to set disksize".to_string()));
        }
        
        // Save loop device info for cleanup
        if let Some(ref loop_dev) = writeback_loop {
            let loop_info_path = format!("{}/zram/writeback_loop", WORK_DIR);
            let _ = std::fs::write(&loop_info_path, loop_dev);
        }
    } else {
        // Normal mode: use zramctl
        info!("Zram: trying to initialize free device");
        zram_dev = get_zram_device(zram_alg, zram_size)?;
        info!("Zram: initialized: {}", zram_dev);
    }

    // Run mkswap
    let mkswap_status = Command::new("mkswap")
        .arg(&zram_dev)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if !mkswap_status.success() {
        return Err(ZramError::ZramctlFailed("mkswap failed".to_string()));
    }

    // Generate and start swap unit
    let unit_name = gen_swap_unit(
        Path::new(&zram_dev),
        Some(zram_prio),
        Some("discard"),
        "zram",
    )?;

    systemctl("daemon-reload", "")?;
    systemctl("start", &unit_name)?;

    // Save zram info for writeback operations
    let zram_id = zram_dev.trim_start_matches("/dev/zram");
    let zram_sysfs = format!("/sys/block/zram{}", zram_id);
    let zram_info = format!("{}\n{}", zram_dev, zram_sysfs);
    let _ = std::fs::write(format!("{}/zram/device", WORK_DIR), &zram_info);

    crate::systemd::notify_status("Zram setup finished");
    Ok(())
}

/// Get a free zram device
fn get_zram_device(alg: &str, size: u64) -> Result<String> {
    let mut retries = 3;

    while retries > 0 {
        if retries < 3 {
            warn!("Zram: device or resource was busy, retry #{}", 3 - retries);
            thread::sleep(Duration::from_secs(1));
        }

        let output = Command::new("zramctl")
            .args(["-f", "-a", alg, "-s", &size.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let combined = format!("{} {}", stdout, stderr);

        if combined.contains("failed to reset: Device or resource busy") {
            retries -= 1;
            continue;
        }

        if combined.contains("no free zram device found") {
            warn!("Zram: zramctl can't find free device");
            info!("Zram: using workaround hook for hot add");

            if !Path::new(ZRAM_HOT_ADD).exists() {
                error!(
                    "Zram: this kernel does not support hot adding zram devices, \
                     please use a 4.2+ kernel or see 'modinfo zram'"
                );
                return Err(ZramError::NoFreeDevice);
            }

            let new_zram = read_file(ZRAM_HOT_ADD)?.trim().to_string();
            return Ok(format!("/dev/zram{}", new_zram));
        } else if stdout.starts_with("/dev/zram") {
            return Ok(stdout);
        } else if !output.status.success() {
            return Err(ZramError::ZramctlFailed(combined));
        }

        break;
    }

    if retries == 0 {
        warn!("Zram: device or resource was busy too many times");
        return Err(ZramError::DeviceBusy);
    }

    Err(ZramError::NoFreeDevice)
}

/// Trigger writeback of idle pages from zram to backing device
pub fn writeback_idle() -> Result<()> {
    let device_info = format!("{}/zram/device", WORK_DIR);
    if !Path::new(&device_info).exists() {
        return Ok(());
    }

    let info = std::fs::read_to_string(&device_info)?;
    let lines: Vec<&str> = info.lines().collect();
    if lines.len() < 2 {
        return Ok(());
    }

    let zram_sysfs = lines[1];
    let idle_path = format!("{}/idle", zram_sysfs);
    let writeback_path = format!("{}/writeback", zram_sysfs);

    if !Path::new(&writeback_path).exists() {
        return Ok(());
    }

    // Mark all pages as idle
    if let Err(e) = std::fs::write(&idle_path, "all") {
        warn!("Zram: failed to mark pages idle: {}", e);
        return Ok(());
    }

    // Trigger writeback of idle pages
    info!("Zram: triggering writeback of idle pages");
    if let Err(e) = std::fs::write(&writeback_path, "idle") {
        warn!("Zram: writeback failed: {}", e);
    }

    Ok(())
}

/// Release a zram device
pub fn release(device: &str) -> Result<()> {
    let status = Command::new("zramctl")
        .args(["-r", device])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(ZramError::ZramctlFailed(format!(
            "Failed to release {}",
            device
        )))
    }
}
