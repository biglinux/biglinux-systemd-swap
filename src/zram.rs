// Zram configuration for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::config::{Config, WORK_DIR};
use crate::helpers::{get_fstype, makedirs, read_file};
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

/// Filesystems that support writeback backing files
const WRITEBACK_SUPPORTED_FS: &[&str] = &["btrfs", "ext4", "xfs"];



/// Check if a filesystem supports persistent files for writeback
fn supports_writeback(fstype: &Option<String>) -> bool {
    match fstype {
        Some(fs) => WRITEBACK_SUPPORTED_FS.contains(&fs.as_str()),
        None => false,
    }
}

/// Create backing file and loop device for zram writeback
fn create_writeback_backing(config: &Config) -> Result<String> {
    // Use same path as swapfile for consistency
    let swapfile_path = config.get("swapfile_path").unwrap_or("/swapfile");
    let parent = Path::new(swapfile_path).parent().unwrap_or(Path::new("/swapfile"));
    
    // Check filesystem type - don't allow writeback on tmpfs, squashfs (LiveCD), etc.
    let fstype = get_fstype(parent);
    if !supports_writeback(&fstype) {
        let fs_name = fstype.unwrap_or_else(|| "unknown".to_string());
        warn!("Zram: writeback disabled - unsupported filesystem '{}' (requires btrfs/ext4/xfs)", fs_name);
        warn!("Zram: this is expected on LiveCD, tmpfs, or network filesystems");
        return Err(ZramError::ZramctlFailed(format!(
            "Writeback not supported on {} filesystem", fs_name
        )));
    }
    
    makedirs(parent)?;
    
    let backing_file = parent.join("zram_writeback");
    
    // Remove if exists
    if backing_file.exists() {
        std::fs::remove_file(&backing_file)?;
    }
    
    // Create pre-allocated file (1GB default)
    // Must use fallocate (not truncate) because sparse files cause I/O errors
    // when zram tries to write to unallocated holes in the backing device
    let size = config.get("zram_writeback_size").unwrap_or("1G");
    let size_bytes = parse_zram_size(size)?;
    
    info!("Zram: creating writeback backing file: {} ({} bytes)", backing_file.display(), size_bytes);
    
    // Create file with correct permissions
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&backing_file)?;
    }
    
    // Use fallocate to pre-allocate the full space (avoids sparse file issues)
    let falloc_status = Command::new("fallocate")
        .args(["-l", &size_bytes.to_string()])
        .arg(&backing_file)
        .status()?;
    if !falloc_status.success() {
        // Fallback to truncate if fallocate is not supported (e.g., some filesystems)
        warn!("Zram: fallocate failed, falling back to truncate (sparse file)");
        Command::new("truncate")
            .args(["--size", &size_bytes.to_string()])
            .arg(&backing_file)
            .status()?;
    }
    
    // Create loop device with Direct I/O enabled
    // Direct I/O avoids double caching: zram data shouldn't go through page cache
    let output = Command::new("losetup")
        .args(["-f", "--show", "--direct-io=on"])
        .arg(&backing_file)
        .stdout(Stdio::piped())
        .output()?;
    
    if !output.status.success() {
        return Err(ZramError::ZramctlFailed("losetup failed".to_string()));
    }
    
    let loop_dev = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!("Zram: created loop device {} for writeback (direct-io=on)", loop_dev);
    
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
    let zram_size = parse_zram_size(config.get("zram_size").unwrap_or("80%"))?;
    let zram_alg = config.get("zram_alg").unwrap_or("zstd");  // zstd offers best compression with acceptable speed
    let zram_prio: i32 = config.get_as("zram_prio").unwrap_or(32767);
    let zram_writeback = config.get_bool("zram_writeback");
    let zram_writeback_dev = config.get("zram_writeback_dev").unwrap_or("");
    
    // mem_limit: maximum real RAM that zram can use (including overhead)
    // This protects against OOM when compression ratio is poor
    // Default: 70% of total RAM (safe balance between capacity and protection)
    let zram_mem_limit = config.get_opt("zram_mem_limit")
        .and_then(|s| parse_zram_size(s).ok())
        .unwrap_or_else(|| {
            // Default to 70% of RAM if not specified
            crate::meminfo::get_ram_size().unwrap_or(0) * 70 / 100
        });

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
        
        // Best-effort cleanup closure: release the zram device on setup failure
        let cleanup_device = |id: &str| {
            let reset_path = format!("/sys/block/zram{}/reset", id);
            let _ = std::fs::write(&reset_path, "1");
        };
        
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
            cleanup_device(&new_id);
            return Err(ZramError::ZramctlFailed("Failed to set disksize".to_string()));
        }
        
        // Set mem_limit to protect real RAM usage
        // This is critical for preventing OOM when compression is poor
        if zram_mem_limit > 0 {
            let mem_limit_path = format!("{}/mem_limit", zram_sysfs);
            if Path::new(&mem_limit_path).exists() {
                match std::fs::write(&mem_limit_path, zram_mem_limit.to_string()) {
                    Ok(_) => info!("Zram: mem_limit = {} MiB (RAM protection)", zram_mem_limit / (1024 * 1024)),
                    Err(e) => warn!("Zram: failed to set mem_limit: {}", e),
                }
            }
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
        
        // Set mem_limit for normal mode too
        if zram_mem_limit > 0 {
            let zram_id = zram_dev.trim_start_matches("/dev/zram");
            let mem_limit_path = format!("/sys/block/zram{}/mem_limit", zram_id);
            if Path::new(&mem_limit_path).exists() {
                match std::fs::write(&mem_limit_path, zram_mem_limit.to_string()) {
                    Ok(_) => info!("Zram: mem_limit = {} MiB (RAM protection)", zram_mem_limit / (1024 * 1024)),
                    Err(e) => warn!("Zram: failed to set mem_limit: {}", e),
                }
            }
        }
    }

    // Configure recompression (kernel 6.1+)
    // Secondary algorithm recompresses idle/huge pages for better ratio
    {
        let zram_id = zram_dev.trim_start_matches("/dev/zram");
        let recomp_alg_path = format!("/sys/block/zram{}/recomp_algorithm", zram_id);
        if Path::new(&recomp_alg_path).exists() {
            let recomp_alg = config.get("zram_recomp_alg").unwrap_or("zstd");
            let recomp_str = format!("algo={} priority=1", recomp_alg);
            match std::fs::write(&recomp_alg_path, &recomp_str) {
                Ok(_) => info!("Zram: recompression algorithm = {} (for idle/huge pages)", recomp_alg),
                Err(e) => warn!("Zram: failed to set recomp_algorithm: {}", e),
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

/// Check if zram recompression is supported (kernel 6.1+)
pub fn is_recompression_available() -> bool {
    let device_info = format!("{}/zram/device", WORK_DIR);
    if let Ok(info) = std::fs::read_to_string(&device_info) {
        let lines: Vec<&str> = info.lines().collect();
        if lines.len() >= 2 {
            let recompress_path = format!("{}/recompress", lines[1]);
            return Path::new(&recompress_path).exists();
        }
    }
    false
}

/// Trigger recompression of idle zram pages with secondary algorithm
/// Kernel 6.1+ feature: pages compressed with primary algo get recompressed
/// with a secondary (better ratio) algo when idle, saving RAM.
pub fn recompress_idle(threshold: u32) -> Result<()> {
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
    let recompress_path = format!("{}/recompress", zram_sysfs);

    if !Path::new(&recompress_path).exists() {
        return Ok(());
    }

    // Mark all pages as idle
    if let Err(e) = std::fs::write(&idle_path, "all") {
        warn!("Zram: recompress: failed to mark pages idle: {}", e);
        return Ok(());
    }

    // Small delay to let kernel update idle state
    thread::sleep(Duration::from_millis(50));

    // Recompress huge pages first (poorly compressed, most benefit)
    if let Err(e) = std::fs::write(&recompress_path, "type=huge") {
        // EINVAL if no huge pages — normal
        if !e.to_string().contains("Invalid argument") {
            warn!("Zram: huge recompression failed: {}", e);
        }
    }

    // Recompress idle pages above threshold (in bytes of compressed size)
    // Pages whose compressed size exceeds threshold get recompressed with
    // the secondary algorithm for better ratio
    let cmd = format!("type=idle threshold={}", threshold);
    match std::fs::write(&recompress_path, &cmd) {
        Ok(_) => info!("Zram: recompressed idle pages (threshold={}B)", threshold),
        Err(e) => {
            if !e.to_string().contains("Invalid argument") {
                warn!("Zram: idle recompression failed: {}", e);
            }
        }
    }

    Ok(())
}

/// Get zram usage statistics for intelligent writeback decisions
pub fn get_zram_stats() -> Option<ZramStats> {
    let device_info = format!("{}/zram/device", WORK_DIR);
    if !Path::new(&device_info).exists() {
        return None;
    }

    let info = std::fs::read_to_string(&device_info).ok()?;
    let lines: Vec<&str> = info.lines().collect();
    if lines.len() < 2 {
        return None;
    }

    let zram_sysfs = lines[1];
    
    // Read mm_stat for comprehensive stats
    let mm_stat_path = format!("{}/mm_stat", zram_sysfs);
    let mm_stat = std::fs::read_to_string(&mm_stat_path).ok()?;
    let fields: Vec<u64> = mm_stat
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    
    // mm_stat format (kernel 4.7+):
    // orig_data_size compr_data_size mem_used_total mem_limit mem_used_max 
    // same_pages pages_compacted huge_pages
    if fields.len() < 5 {
        return None;
    }

    // Read disksize
    let disksize_path = format!("{}/disksize", zram_sysfs);
    let disksize: u64 = std::fs::read_to_string(&disksize_path)
        .ok()?
        .trim()
        .parse()
        .ok()?;

    // Read bd_stat for writeback stats (if available)
    let bd_stat_path = format!("{}/bd_stat", zram_sysfs);
    let (bd_count, bd_reads, bd_writes) = if Path::new(&bd_stat_path).exists() {
        let bd_stat = std::fs::read_to_string(&bd_stat_path).unwrap_or_default();
        let bd_fields: Vec<u64> = bd_stat
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if bd_fields.len() >= 3 {
            (bd_fields[0], bd_fields[1], bd_fields[2])
        } else {
            (0, 0, 0)
        }
    } else {
        (0, 0, 0)
    };

    Some(ZramStats {
        orig_data_size: fields[0],
        compr_data_size: fields[1],
        mem_used_total: fields[2],
        disksize,
        same_pages: fields.get(5).copied().unwrap_or(0),
        pages_compacted: fields.get(6).copied().unwrap_or(0),
        bd_count,
        bd_reads,
        bd_writes,
    })
}

/// Zram statistics for monitoring
#[derive(Debug, Clone)]
pub struct ZramStats {
    /// Original uncompressed data size
    pub orig_data_size: u64,
    /// Compressed data size
    pub compr_data_size: u64,
    /// Total memory used (including allocator overhead)
    pub mem_used_total: u64,
    /// Configured disk size
    pub disksize: u64,
    /// Pages that were same-page merged
    pub same_pages: u64,
    /// Pages compacted
    pub pages_compacted: u64,
    /// Backing device: pages stored
    pub bd_count: u64,
    /// Backing device: read operations
    pub bd_reads: u64,
    /// Backing device: write operations
    pub bd_writes: u64,
}

impl ZramStats {
    /// Calculate compression ratio (uncompressed / compressed)
    pub fn compression_ratio(&self) -> f64 {
        if self.compr_data_size == 0 {
            0.0
        } else {
            self.orig_data_size as f64 / self.compr_data_size as f64
        }
    }

    /// Calculate memory utilization percentage
    pub fn memory_utilization(&self) -> u8 {
        if self.disksize == 0 {
            0
        } else {
            ((self.orig_data_size as f64 / self.disksize as f64) * 100.0) as u8
        }
    }
}

/// Configuration for smart zram writeback
#[derive(Debug, Clone)]
pub struct ZramWritebackConfig {
    /// Minimum zram utilization % before triggering writeback (default: 50)
    pub writeback_threshold: u8,
    /// Write back when compression ratio drops below this (default: 2.0)
    /// Pages that don't compress well should go to disk
    pub min_compression_ratio: f64,
    /// Idle time in seconds before marking pages for writeback (default: 120)
    pub idle_age_seconds: u64,
    /// Check interval in seconds (default: 30)
    pub check_interval: u64,
    /// Enable huge page writeback (incompressible huge pages go to disk)
    pub writeback_huge: bool,
    /// Enable backing file expansion (like swapfc)
    pub auto_expand_backing: bool,
    /// Backing file expansion step size
    pub expand_step_bytes: u64,
    /// Maximum backing file size
    pub max_backing_size: u64,
    /// Enable recompression of idle pages (kernel 6.1+)
    pub recompress_enabled: bool,
    /// Recompression threshold: recompress idle pages with compressed size above this (bytes)
    /// Default 3072 = pages that compressed to >75% of page size (poor compression)
    pub recompress_threshold: u32,
    /// Interval in seconds between recompression attempts (default: 120)
    pub recompress_interval: u64,
}

impl Default for ZramWritebackConfig {
    fn default() -> Self {
        Self {
            writeback_threshold: 50,
            min_compression_ratio: 2.0,
            idle_age_seconds: 120,
            check_interval: 30,
            writeback_huge: true,
            auto_expand_backing: true,
            expand_step_bytes: 512 * 1024 * 1024,  // 512MB
            max_backing_size: 8 * 1024 * 1024 * 1024,  // 8GB
            recompress_enabled: true,  // Enable by default on supported kernels
            recompress_threshold: 3072,  // Recompress pages >3KB compressed (poor ratio)
            recompress_interval: 120,  // Every 2 minutes
        }
    }
}

impl ZramWritebackConfig {
    pub fn from_config(config: &Config) -> Self {
        let mut cfg = Self::default();
        
        if let Ok(v) = config.get_as::<u8>("zram_writeback_threshold") {
            cfg.writeback_threshold = v.clamp(20, 90);
        }
        if let Ok(v) = config.get_as::<f64>("zram_writeback_min_ratio") {
            cfg.min_compression_ratio = v.clamp(1.0, 5.0);
        }
        if let Ok(v) = config.get_as::<u64>("zram_writeback_idle_age") {
            cfg.idle_age_seconds = v.clamp(30, 3600);
        }
        if let Ok(v) = config.get_as::<u64>("zram_writeback_interval") {
            cfg.check_interval = v.clamp(10, 300);
        }
        cfg.writeback_huge = !config.get_bool("zram_writeback_no_huge");
        cfg.auto_expand_backing = !config.get_bool("zram_writeback_no_expand");
        
        if let Ok(size_str) = config.get("zram_writeback_max_size") {
            if let Ok(size) = parse_zram_size(size_str) {
                cfg.max_backing_size = size;
            }
        }
        
        // Recompression config
        if config.get_bool("zram_recompress_disabled") {
            cfg.recompress_enabled = false;
        }
        if let Ok(v) = config.get_as::<u32>("zram_recompress_threshold") {
            cfg.recompress_threshold = v.clamp(512, 4096);
        }
        if let Ok(v) = config.get_as::<u64>("zram_recompress_interval") {
            cfg.recompress_interval = v.clamp(30, 600);
        }
        
        cfg
    }
}

/// Smart writeback manager for zram
pub struct ZramWritebackManager {
    config: ZramWritebackConfig,
    backing_file: Option<String>,
    current_backing_size: u64,
    recompress_counter: u64,
}

impl ZramWritebackManager {
    pub fn new(config: ZramWritebackConfig) -> Self {
        // Try to get existing backing file info
        let backing_file = std::fs::read_to_string(format!("{}/zram/writeback_file", WORK_DIR))
            .ok()
            .map(|s| s.trim().to_string());
        
        let current_backing_size = backing_file.as_ref()
            .and_then(|f| std::fs::metadata(f).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        
        Self {
            config,
            backing_file,
            current_backing_size,
            recompress_counter: 0,
        }
    }

    /// Check if writeback is available
    pub fn is_available(&self) -> bool {
        let device_info = format!("{}/zram/device", WORK_DIR);
        if !Path::new(&device_info).exists() {
            return false;
        }
        
        if let Ok(info) = std::fs::read_to_string(&device_info) {
            let lines: Vec<&str> = info.lines().collect();
            if lines.len() >= 2 {
                let writeback_path = format!("{}/writeback", lines[1]);
                return Path::new(&writeback_path).exists();
            }
        }
        false
    }

    /// Run the smart writeback monitoring loop
    pub fn run(&mut self) -> Result<()> {
        if !self.is_available() {
            // Even without writeback, we can still do recompression
            if self.config.recompress_enabled && is_recompression_available() {
                info!("Zram: writeback not available, running recompression-only mode");
                return self.run_recompress_only();
            }
            info!("Zram: writeback and recompression not available, skipping");
            return Ok(());
        }

        info!("Zram: starting smart writeback manager");
        info!("Zram:   threshold={}%, min_ratio={:.1}, idle_age={}s",
            self.config.writeback_threshold,
            self.config.min_compression_ratio,
            self.config.idle_age_seconds);
        if self.config.recompress_enabled && is_recompression_available() {
            info!("Zram:   recompression enabled (threshold={}B, interval={}s)",
                self.config.recompress_threshold, self.config.recompress_interval);
        }
        
        loop {
            thread::sleep(Duration::from_secs(self.config.check_interval));

            if crate::is_shutdown() {
                break;
            }

            if let Some(stats) = get_zram_stats() {
                self.process_stats(&stats);
            }
        }

        Ok(())
    }

    /// Run recompression-only loop (when writeback is not available)
    fn run_recompress_only(&mut self) -> Result<()> {
        info!("Zram: recompression-only mode (threshold={}B, interval={}s)",
            self.config.recompress_threshold, self.config.recompress_interval);

        loop {
            thread::sleep(Duration::from_secs(self.config.recompress_interval));

            if crate::is_shutdown() {
                break;
            }

            if let Some(stats) = get_zram_stats() {
                // Only recompress if there's meaningful data in zram
                if stats.orig_data_size > 0 {
                    let _ = recompress_idle(self.config.recompress_threshold);
                }
            }
        }

        Ok(())
    }

    /// Process zram stats and decide on writeback actions
    fn process_stats(&mut self, stats: &ZramStats) {
        let utilization = stats.memory_utilization();
        let ratio = stats.compression_ratio();

        // Check if we should trigger writeback
        let should_writeback = utilization >= self.config.writeback_threshold
            || ratio < self.config.min_compression_ratio;

        if should_writeback {
            info!("Zram: util={}%, ratio={:.2}x - triggering writeback", utilization, ratio);
            
            // First, write back huge/incompressible pages (most benefit)
            if self.config.writeback_huge {
                self.writeback_huge();
            }
            
            // Then write back idle pages
            self.writeback_with_age();
            
            // Check if we need to expand backing
            if self.config.auto_expand_backing {
                self.maybe_expand_backing(stats);
            }
        }

        // Recompression: runs on its own interval (independent of writeback threshold)
        if self.config.recompress_enabled && is_recompression_available() {
            let elapsed = self.recompress_counter * self.config.check_interval;
            if elapsed >= self.config.recompress_interval {
                self.recompress_counter = 0;
                if stats.orig_data_size > 0 {
                    let _ = recompress_idle(self.config.recompress_threshold);
                }
            } else {
                self.recompress_counter += 1;
            }
        }
    }

    /// Write back huge pages (incompressible)
    fn writeback_huge(&self) {
        let device_info = format!("{}/zram/device", WORK_DIR);
        if let Ok(info) = std::fs::read_to_string(&device_info) {
            let lines: Vec<&str> = info.lines().collect();
            if lines.len() >= 2 {
                let writeback_path = format!("{}/writeback", lines[1]);
                if let Err(e) = std::fs::write(&writeback_path, "huge") {
                    // This fails if no huge pages - that's OK
                    if !e.to_string().contains("Invalid argument") {
                        warn!("Zram: huge writeback failed: {}", e);
                    }
                }
            }
        }
    }

    /// Write back pages older than configured age
    fn writeback_with_age(&self) {
        let device_info = format!("{}/zram/device", WORK_DIR);
        if let Ok(info) = std::fs::read_to_string(&device_info) {
            let lines: Vec<&str> = info.lines().collect();
            if lines.len() >= 2 {
                let zram_sysfs = lines[1];
                let idle_path = format!("{}/idle", zram_sysfs);
                let writeback_path = format!("{}/writeback", zram_sysfs);

                // Mark pages as idle based on age
                // The kernel tracks access time and respects the idle marking
                if let Err(e) = std::fs::write(&idle_path, "all") {
                    warn!("Zram: failed to mark idle: {}", e);
                    return;
                }

                // Small delay to let kernel update idle state
                thread::sleep(Duration::from_millis(100));

                // Write back idle pages
                if let Err(e) = std::fs::write(&writeback_path, "idle") {
                    if !e.to_string().contains("Invalid argument") {
                        warn!("Zram: idle writeback failed: {}", e);
                    }
                }
            }
        }
    }

    /// Expand backing file if needed
    fn maybe_expand_backing(&mut self, stats: &ZramStats) {
        let backing_file = match &self.backing_file {
            Some(f) => f.clone(),
            None => return,
        };

        // Check if backing is getting full (bd_count is pages on backing)
        let page_size = crate::meminfo::get_page_size();
        let backing_used = stats.bd_count * page_size;
        let backing_usage_percent = if self.current_backing_size > 0 {
            (backing_used as f64 / self.current_backing_size as f64 * 100.0) as u8
        } else {
            0
        };

        // Expand if backing is > 75% full and under max size
        if backing_usage_percent > 75 && self.current_backing_size < self.config.max_backing_size {
            let new_size = (self.current_backing_size + self.config.expand_step_bytes)
                .min(self.config.max_backing_size);
            
            info!("Zram: expanding backing file from {}MB to {}MB ({}% full)",
                self.current_backing_size / (1024 * 1024),
                new_size / (1024 * 1024),
                backing_usage_percent);

            // Expand with fallocate to pre-allocate space (avoids sparse file holes)
            let status = Command::new("fallocate")
                .args(["-l", &new_size.to_string()])
                .arg(&backing_file)
                .status();

            if let Ok(s) = status {
                if s.success() {
                    self.current_backing_size = new_size;
                    info!("Zram: backing file expanded successfully");
                }
            }
        }
    }
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

