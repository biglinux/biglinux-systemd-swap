// Zswap configuration for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use thiserror::Error;

use crate::config::{Config, WORK_DIR};
use crate::helpers::{makedirs, read_file, write_file};
use crate::{error, info, warn};

const ZSWAP_MODULE: &str = "/sys/module/zswap";
const ZSWAP_PARAMS: &str = "/sys/module/zswap/parameters";

#[derive(Error, Debug)]
pub enum ZswapError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Helper error: {0}")]
    Helper(#[from] crate::helpers::HelperError),
    #[error("Zswap not supported on this kernel")]
    NotSupported,
}

pub type Result<T> = std::result::Result<T, ZswapError>;

/// Backup of original zswap parameters
#[derive(Debug, Clone)]
pub struct ZswapBackup {
    pub parameters: HashMap<String, String>,
}

impl ZswapBackup {
    /// Restore original zswap parameters
    pub fn restore(&self) -> Result<()> {
        info!("Zswap: restore configuration: start");
        for (path, value) in &self.parameters {
            if let Err(e) = write_file(path, value) {
                warn!("Failed to restore {}: {}", path, e);
            }
        }
        info!("Zswap: restore configuration: complete");
        Ok(())
    }
}

/// Check if zswap is available (module loaded)
pub fn is_available() -> bool {
    Path::new(ZSWAP_MODULE).is_dir()
}

/// Check if zswap is currently enabled
pub fn is_enabled() -> bool {
    let enabled_path = format!("{}/enabled", ZSWAP_PARAMS);
    if let Ok(content) = read_file(&enabled_path) {
        let value = content.trim();
        return value == "Y" || value == "1";
    }
    false
}

/// Enable or disable zswap
fn set_enabled(enable: bool) -> Result<()> {
    let enabled_path = format!("{}/enabled", ZSWAP_PARAMS);
    let value = if enable { "1" } else { "0" };
    write_file(&enabled_path, value)?;
    info!("Zswap: {} zswap", if enable { "enabled" } else { "disabled" });
    Ok(())
}

/// Start and configure zswap
pub fn start(config: &Config) -> Result<ZswapBackup> {
    crate::systemd::notify_status("Setting up Zswap...");

    if !is_available() {
        return Err(ZswapError::NotSupported);
    }

    info!("Zswap: backup current configuration: start");
    makedirs(format!("{}/zswap", WORK_DIR))?;

    // Backup current parameters
    let mut backup = HashMap::new();
    if let Ok(entries) = fs::read_dir(ZSWAP_PARAMS) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(content) = read_file(&path) {
                    backup.insert(path.to_string_lossy().to_string(), content);
                }
            }
        }
    }
    info!("Zswap: backup current configuration: complete");

    // Get config values
    // Default to "1" because if start() is called, zswap should be enabled
    let enabled = config.get("zswap_enabled").unwrap_or("1");
    let compressor = config.get("zswap_compressor").unwrap_or("lz4");  // LZ4 for speed
    let max_pool_percent = config.get("zswap_max_pool_percent").unwrap_or("50");  // Unified 50%
    let zpool = config.get("zswap_zpool").unwrap_or("zsmalloc");  // zsmalloc (z3fold removed in newer kernels)
    let shrinker_enabled = config.get("zswap_shrinker_enabled").unwrap_or("1");  // Enable shrinker
    let accept_threshold = config.get("zswap_accept_threshold").unwrap_or("90");

    info!(
        "Zswap: Enable: {}, Comp: {}, Max pool %: {}, Zpool: {}, Shrinker: {}, Accept threshold: {}%",
        enabled, compressor, max_pool_percent, zpool, shrinker_enabled, accept_threshold
    );

    info!("Zswap: set new parameters: start");

    // IMPORTANT: Some parameters (compressor, zpool) cannot be changed while zswap is enabled.
    // We must disable zswap first, configure parameters, then re-enable.
    let was_enabled = is_enabled();
    if was_enabled {
        info!("Zswap: temporarily disabling to change parameters");
        if let Err(e) = set_enabled(false) {
            warn!("Zswap: failed to disable temporarily: {}", e);
        }
    }

    // Write parameters (except enabled) - order matters for some kernels
    // Configure zpool and compressor first, then max_pool_percent
    let params = [
        ("zpool", zpool),
        ("compressor", compressor),
        ("max_pool_percent", max_pool_percent),
        ("shrinker_enabled", shrinker_enabled),
        ("accept_threshold_percent", accept_threshold),
    ];

    for (name, value) in params {
        let path = format!("{}/{}", ZSWAP_PARAMS, name);
        if let Err(e) = write_file(&path, value) {
            // shrinker_enabled may not exist on older kernels, just warn
            if name == "shrinker_enabled" || name == "accept_threshold_percent" {
                warn!("Zswap: {} not supported on this kernel", name);
            } else {
                error!("Failed to write zswap_{}: {}", name, e);
            }
        }
    }

    // Now enable zswap if requested
    let should_enable = enabled == "1" || enabled.to_lowercase() == "y" || enabled.to_lowercase() == "yes";
    if should_enable {
        if let Err(e) = set_enabled(true) {
            error!("Failed to enable zswap: {}", e);
        }
    } else if was_enabled {
        // If it was enabled before but config says disabled, keep it disabled
        info!("Zswap: keeping disabled as per configuration");
    }

    info!("Zswap: set new parameters: complete");

    Ok(ZswapBackup { parameters: backup })
}

/// Get zswap status information
pub fn get_status() -> Option<ZswapStatus> {
    if !is_available() {
        return None;
    }

    let params_dir = Path::new(ZSWAP_PARAMS);
    let debug_dir = Path::new("/sys/kernel/debug/zswap");

    let mut status = ZswapStatus::default();

    // Read parameters
    if let Ok(v) = read_file(params_dir.join("enabled")) {
        status.enabled = v.trim() == "Y" || v.trim() == "1";
    }
    if let Ok(v) = read_file(params_dir.join("compressor")) {
        status.compressor = v.trim().to_string();
    }
    if let Ok(v) = read_file(params_dir.join("zpool")) {
        status.zpool = v.trim().to_string();
    }
    if let Ok(v) = read_file(params_dir.join("max_pool_percent")) {
        status.max_pool_percent = v.trim().parse().unwrap_or(20);
    }
    if let Ok(v) = read_file(params_dir.join("shrinker_enabled")) {
        status.shrinker_enabled = v.trim() == "Y" || v.trim() == "1";
    }
    if let Ok(v) = read_file(params_dir.join("accept_threshold_percent")) {
        status.accept_threshold_percent = v.trim().parse().unwrap_or(90);
    }

    // Read debug stats (requires root)
    if debug_dir.is_dir() {
        let read_stat = |name: &str| -> u64 {
            read_file(debug_dir.join(name))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        };

        status.pool_size = read_stat("pool_total_size");
        status.stored_pages = read_stat("stored_pages");
        status.written_back_pages = read_stat("written_back_pages");
        status.reject_reclaim_fail = read_stat("reject_reclaim_fail");
        status.same_filled_pages = read_stat("same_filled_pages");
        status.pool_limit_hit = read_stat("pool_limit_hit");
        status.duplicate_entry = read_stat("duplicate_entry");
    }

    Some(status)
}

/// Zswap status information
#[derive(Debug, Default)]
pub struct ZswapStatus {
    // Configuration parameters
    pub enabled: bool,
    pub compressor: String,
    pub zpool: String,
    pub max_pool_percent: u8,
    pub shrinker_enabled: bool,
    pub accept_threshold_percent: u8,

    // Runtime statistics (from debugfs, requires root)
    /// Total bytes used by zswap pool in RAM
    pub pool_size: u64,
    /// Pages currently stored in zswap pool
    pub stored_pages: u64,
    /// Pages written back to backing swap device
    pub written_back_pages: u64,
    /// Pages rejected due to reclaim failure
    pub reject_reclaim_fail: u64,
    /// Same-value filled pages (zeros, etc)
    pub same_filled_pages: u64,
    /// Number of times pool limit was hit
    pub pool_limit_hit: u64,
    /// Duplicate entries found
    pub duplicate_entry: u64,
}

impl ZswapStatus {
    /// Calculate pool utilization percentage
    pub fn pool_utilization_percent(&self, mem_total: u64) -> u8 {
        if mem_total == 0 || self.max_pool_percent == 0 {
            return 0;
        }
        let max_pool_size = mem_total * self.max_pool_percent as u64 / 100;
        if max_pool_size == 0 {
            return 0;
        }
        ((self.pool_size * 100) / max_pool_size).min(100) as u8
    }

    /// Calculate compression ratio (compressed/uncompressed)
    pub fn compression_ratio(&self, page_size: u64) -> f64 {
        let uncompressed = self.stored_pages * page_size;
        if uncompressed == 0 {
            return 0.0;
        }
        self.pool_size as f64 / uncompressed as f64
    }
}

