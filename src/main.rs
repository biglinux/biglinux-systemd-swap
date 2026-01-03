// systemd-swap - Dynamic swap management for Linux
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};

use systemd_swap::config::{Config, WORK_DIR};
use systemd_swap::helpers::{am_i_root, find_swap_units, force_remove, get_what_from_swap_unit, makedirs, read_file};
use systemd_swap::meminfo::{get_mem_stats, get_page_size};
use systemd_swap::swapfc::SwapFc;
use systemd_swap::systemd::{notify_ready, notify_status, notify_stopping, swapoff, systemctl};
use systemd_swap::zswap::ZswapBackup;
use systemd_swap::{error, info, request_shutdown, warn};

#[derive(Parser)]
#[command(name = "systemd-swap")]
#[command(about = "Dynamic swap management for zram, zswap, and swap files")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the swap management daemon
    Start,
    /// Stop the swap management daemon
    Stop,
    /// Show swap status information
    Status,
    /// List available compression algorithms
    Compression,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command.unwrap_or(Commands::Status) {
        Commands::Start => start(),
        Commands::Stop => stop(false),
        Commands::Status => status(),
        Commands::Compression => compression(),
    };

    if let Err(e) = result {
        error!("{}", e);
        std::process::exit(1);
    }
}

/// Start the swap daemon
fn start() -> Result<(), Box<dyn std::error::Error>> {
    am_i_root()?;

    // Clean up any previous instance
    let _ = stop(true);

    // Initialize directories
    makedirs(WORK_DIR)?;
    makedirs(format!("{}/system/local-fs.target.wants", systemd_swap::config::RUN_SYSD))?;
    makedirs(format!("{}/system/swap.target.wants", systemd_swap::config::RUN_SYSD))?;

    let config = Config::load()?;

    // Warn about incompatible configurations
    if config.get_bool("zram_enabled")
        && (config.get_bool("zswap_enabled")
            || config.get_bool("swapfc_enabled")
            || config.get_bool("swapd_auto_swapon"))
    {
        warn!(
            "Combining zram with zswap/swapfc/swapd_auto_swapon can lead to LRU \
             inversion and is strongly recommended against"
        );
    }

    // Store zswap backup for restoration
    let mut zswap_backup: Option<ZswapBackup> = None;

    // Start zswap
    if config.get_bool("zswap_enabled") {
        match systemd_swap::zswap::start(&config) {
            Ok(backup) => zswap_backup = Some(backup),
            Err(e) => error!("Zswap: {}", e),
        }
    }

    // Start zram
    if config.get_bool("zram_enabled") {
        if let Err(e) = systemd_swap::zram::start(&config) {
            error!("Zram: {}", e);
        }
    }

    // Save destroy info (simplified - just zswap backup)
    if let Some(ref backup) = zswap_backup {
        let backup_path = format!("{}/zswap_backup", WORK_DIR);
        makedirs(&backup_path)?;
        for (path, value) in &backup.parameters {
            let filename = Path::new(path).file_name().unwrap_or_default();
            let save_path = format!("{}/{}", backup_path, filename.to_string_lossy());
            fs::write(&save_path, format!("{}={}", path, value))?;
        }
    }

    // Start swapd (auto swapon)
    if config.get_bool("swapd_auto_swapon") {
        start_swapd(&config)?;
    }

    // Start swapfc (dynamic swap files)
    if config.get_bool("swapfc_enabled") {
        // Set up signal handler
        signal_hook::flag::register(signal_hook::consts::SIGTERM, std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))?;
        
        ctrlc::set_handler(move || {
            request_shutdown();
        })?;

        let mut swapfc = SwapFc::new(&config)?;
        swapfc.run()?;
    } else {
        notify_ready();
        info!("Swap setup complete, no monitoring needed");
    }

    Ok(())
}

/// Start swapd (auto-enable swap partitions)
fn start_swapd(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    notify_status("Activating swap units...");
    info!("swapD: searching swap devices");

    makedirs(format!("{}/swapd", WORK_DIR))?;

    let prio: i32 = config.get_as("swapd_prio").unwrap_or(1024);

    // Find swap partitions
    let output = Command::new("blkid")
        .args(["-t", "TYPE=swap", "-o", "device"])
        .stdout(Stdio::piped())
        .output()?;

    let devices = String::from_utf8_lossy(&output.stdout);

    for device in devices.lines() {
        let device = device.trim();
        if device.is_empty() || device.contains("zram") || device.contains("loop") {
            continue;
        }

        // Check if already in use
        let swapon_output = Command::new("swapon")
            .args(["--show=NAME", "--noheadings"])
            .stdout(Stdio::piped())
            .output()?;

        let used_devices = String::from_utf8_lossy(&swapon_output.stdout);
        if used_devices.lines().any(|d| d.trim() == device) {
            continue;
        }

        // Generate and start swap unit
        let unit_name = systemd_swap::systemd::gen_swap_unit(
            Path::new(device),
            Some(prio),
            Some("discard"),
            "swapd",
        )?;

        systemctl("daemon-reload", "")?;
        if systemctl("start", &unit_name).is_ok() {
            info!("swapD: enabled device: {}", device);
        }
    }

    notify_status("Swap unit activation finished");
    Ok(())
}

/// Stop the swap daemon
fn stop(on_init: bool) -> Result<(), Box<dyn std::error::Error>> {
    am_i_root()?;

    if !on_init {
        notify_stopping();
    }

    let config = Config::load()?;

    // Stop all managed swap units
    for subsystem in ["swapd", "swapfc", "zram"] {
        for unit_path in find_swap_units() {
            if let Ok(content) = read_file(&unit_path) {
                if content.to_lowercase().contains(subsystem) {
                    if let Some(dev) = get_what_from_swap_unit(&unit_path) {
                        info!("{}: swapoff {}", subsystem, dev);
                        let _ = swapoff(&dev);
                        force_remove(&unit_path, true);

                        if subsystem == "swapfc" && Path::new(&dev).is_file() {
                            force_remove(&dev, true);
                        } else if subsystem == "zram" {
                            let _ = systemd_swap::zram::release(&dev);
                        }
                    }
                }
            }
        }
    }

    // Restore zswap parameters
    let backup_path = format!("{}/zswap_backup", WORK_DIR);
    if Path::new(&backup_path).is_dir() {
        info!("Zswap: restore configuration: start");
        if let Ok(entries) = fs::read_dir(&backup_path) {
            for entry in entries.flatten() {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if let Some((path, value)) = content.split_once('=') {
                        if let Err(e) = fs::write(path, value) {
                            warn!("Failed to restore {}: {}", path, e);
                        }
                    }
                }
            }
        }
        info!("Zswap: restore configuration: complete");
    }

    // Remove work directory
    info!("Removing working directory...");
    let _ = fs::remove_dir_all(WORK_DIR);

    // Remove swap files
    let swapfc_path = config.get("swapfc_path").unwrap_or("/swapfc/swapfile");
    info!("Removing files in {}...", swapfc_path);
    if let Ok(entries) = fs::read_dir(swapfc_path) {
        for entry in entries.flatten() {
            force_remove(entry.path(), true);
        }
    }

    Ok(())
}

/// Show swap status
fn status() -> Result<(), Box<dyn std::error::Error>> {
    if am_i_root().is_err() {
        warn!("Not root! Some output might be missing.");
    }

    let swap_stats = get_mem_stats(&["SwapTotal", "SwapFree"])?;
    let swap_used = swap_stats["SwapTotal"] - swap_stats["SwapFree"];

    // Zswap status
    if let Some(zswap) = systemd_swap::zswap::get_status() {
        println!("Zswap:");
        println!("  enabled: {}", zswap.enabled);
        println!("  compressor: {}", zswap.compressor);
        println!("  zpool: {}", zswap.zpool);

        if zswap.pool_size > 0 || zswap.stored_pages > 0 {
            let page_size = get_page_size();
            let stored_bytes = zswap.stored_pages * page_size;
            let ratio = if zswap.stored_pages > 0 {
                (zswap.pool_size as f64 / stored_bytes as f64) * 100.0
            } else {
                0.0
            };

            println!("  pool_size: {} bytes", zswap.pool_size);
            println!("  stored_pages: {}", zswap.stored_pages);
            println!("  compress_ratio: {:.0}%", ratio);

            if swap_used > 0 {
                let percent = (stored_bytes as f64 / swap_used as f64) * 100.0;
                println!("  zswap_store/swap_store: {}/{} ({:.0}%)", stored_bytes, swap_used, percent);
            }
        }
    }

    // Zram status
    let zramctl_output = Command::new("zramctl")
        .stdout(Stdio::piped())
        .output();

    if let Ok(output) = zramctl_output {
        let output_str = String::from_utf8_lossy(&output.stdout);
        if output_str.contains("[SWAP]") {
            println!("\nZram:");
            for line in output_str.lines() {
                if line.starts_with("NAME") || line.contains("[SWAP]") {
                    let line = line.trim_end_matches("[SWAP]").trim_end_matches("MOUNTPOINT").trim();
                    println!("  {}", line);
                }
            }
        }
    }

    // SwapD status
    if Path::new(&format!("{}/swapd", WORK_DIR)).is_dir() {
        println!("\nswapD:");
        let swapon_output = Command::new("swapon")
            .arg("--raw")
            .stdout(Stdio::piped())
            .output()?;

        for line in String::from_utf8_lossy(&swapon_output.stdout).lines() {
            if !line.contains("zram") && !line.contains("file") && !line.contains("loop") {
                println!("  {}", line);
            }
        }
    }

    // SwapFC status
    if Path::new(&format!("{}/swapfc", WORK_DIR)).is_dir() {
        println!("\nswapFC:");
        let swapon_output = Command::new("swapon")
            .arg("--raw")
            .stdout(Stdio::piped())
            .output()?;

        for line in String::from_utf8_lossy(&swapon_output.stdout).lines() {
            if line.contains("NAME") || line.contains("file") || line.contains("loop") {
                println!("  {}", line);
            }
        }
    }

    Ok(())
}

/// Show available compression algorithms
fn compression() -> Result<(), Box<dyn std::error::Error>> {
    let crypto = fs::read_to_string("/proc/crypto")?;

    print!("Found loaded compression algorithms: ");

    let mut first = true;
    let mut current_name = String::new();

    for line in crypto.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("name") {
            current_name = name.trim_start_matches(':').trim().to_string();
        } else if let Some(typ) = line.strip_prefix("type") {
            let current_type = typ.trim_start_matches(':').trim();

            if current_type == "compression" && !current_name.is_empty() {
                if first {
                    first = false;
                } else {
                    print!(", ");
                }
                print!("{}", current_name);
            }
        }
    }

    println!();
    Ok(())
}
