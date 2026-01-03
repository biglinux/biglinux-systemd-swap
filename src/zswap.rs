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
    let compressor = config.get("zswap_compressor").unwrap_or("lzo");
    let max_pool_percent = config.get("zswap_max_pool_percent").unwrap_or("20");
    let zpool = config.get("zswap_zpool").unwrap_or("zbud");

    info!(
        "Zswap: Enable: {}, Comp: {}, Max pool %: {}, Zpool: {}",
        enabled, compressor, max_pool_percent, zpool
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
    ];

    for (name, value) in params {
        let path = format!("{}/{}", ZSWAP_PARAMS, name);
        if let Err(e) = write_file(&path, value) {
            error!("Failed to write zswap_{}: {}", name, e);
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

    // Read debug stats (may require root)
    if debug_dir.is_dir() {
        if let Ok(v) = read_file(debug_dir.join("pool_total_size")) {
            status.pool_size = v.trim().parse().unwrap_or(0);
        }
        if let Ok(v) = read_file(debug_dir.join("stored_pages")) {
            status.stored_pages = v.trim().parse().unwrap_or(0);
        }
    }

    Some(status)
}

/// Zswap status information
#[derive(Debug, Default)]
pub struct ZswapStatus {
    pub enabled: bool,
    pub compressor: String,
    pub zpool: String,
    pub pool_size: u64,
    pub stored_pages: u64,
}
