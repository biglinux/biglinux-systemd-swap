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

/// Start zram swap
pub fn start(config: &Config) -> Result<()> {
    crate::systemd::notify_status("Setting up Zram...");

    info!("Zram: check module availability");
    if !is_available() {
        return Err(ZramError::NotAvailable);
    }
    info!("Zram: module found!");

    makedirs(format!("{}/zram", WORK_DIR))?;

    let zram_size: u64 = config.get_as("zram_size").unwrap_or(0);
    let zram_alg = config.get("zram_alg").unwrap_or("lzo");
    let zram_prio: i32 = config.get_as("zram_prio").unwrap_or(32767);

    if zram_size == 0 {
        warn!("Zram: size is 0, skipping");
        return Ok(());
    }

    info!("Zram: trying to initialize free device");

    let mut zram_dev: Option<String> = None;
    let mut retries = 3;

    while retries > 0 {
        if retries < 3 {
            warn!("Zram: device or resource was busy, retry #{}", 3 - retries);
            thread::sleep(Duration::from_secs(1));
        }

        let output = Command::new("zramctl")
            .args(["-f", "-a", zram_alg, "-s", &zram_size.to_string()])
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
            zram_dev = Some(format!("/dev/zram{}", new_zram));
            info!("Zram: success: new device {}", zram_dev.as_ref().unwrap());
            break;
        } else if stdout.starts_with("/dev/zram") {
            zram_dev = Some(stdout);
            break;
        } else if !output.status.success() {
            return Err(ZramError::ZramctlFailed(combined));
        }

        break;
    }

    if retries == 0 {
        warn!("Zram: device or resource was busy too many times");
        return Err(ZramError::DeviceBusy);
    }

    let zram_dev = match zram_dev {
        Some(dev) => dev,
        None => {
            warn!("Zram: can't get free zram device");
            return Err(ZramError::NoFreeDevice);
        }
    };

    info!("Zram: initialized: {}", zram_dev);

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

    crate::systemd::notify_status("Zram setup finished");
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
