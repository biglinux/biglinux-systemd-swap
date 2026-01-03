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
use systemd_swap::systemd::{notify_ready, notify_stopping, swapoff};
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
}

/// Swap strategy based on filesystem detection
#[derive(Debug, Clone, Copy, PartialEq)]
enum SwapMode {
    Auto,
    ZramSwapfc,   // zram + writeback to swap files (best for desktop!)
    ZswapSwapfc,  // zswap + swap files (alternative)
    ZramOnly,     // zram only (for non-btrfs)
    Manual,       // Use explicit config values
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Some(Commands::Start) => start(),
        Some(Commands::Stop) => stop(false),
        Some(Commands::Status) => status(),
        None => {
            // No subcommand provided, show help
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            println!();
            return;
        }
    };

    if let Err(e) = result {
        error!("{}", e);
        std::process::exit(1);
    }
}

/// Detect if root filesystem is btrfs
fn is_root_btrfs() -> bool {
    let output = Command::new("df")
        .args(["--output=fstype", "/"])
        .stdout(Stdio::piped())
        .output();

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(fstype) = stdout.lines().nth(1) {
            return fstype.trim() == "btrfs";
        }
    }
    false
}

/// Detect if swapfc_path is on btrfs
fn is_path_btrfs(path: &str) -> bool {
    // Check the parent directory or the path itself
    let check_path = if Path::new(path).exists() {
        path.to_string()
    } else if let Some(parent) = Path::new(path).parent() {
        parent.to_string_lossy().to_string()
    } else {
        "/".to_string()
    };

    let output = Command::new("df")
        .args(["--output=fstype", &check_path])
        .stdout(Stdio::piped())
        .output();

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(fstype) = stdout.lines().nth(1) {
            return fstype.trim() == "btrfs";
        }
    }
    false
}

/// Parse swap_mode from config
fn get_swap_mode(config: &Config) -> SwapMode {
    match config.get("swap_mode").unwrap_or("auto").to_lowercase().as_str() {
        "zram+swapfc" | "zram_swapfc" => SwapMode::ZramSwapfc,
        "zswap+swapfc" | "zswap" => SwapMode::ZswapSwapfc,
        "zram" | "zram_only" => SwapMode::ZramOnly,
        "manual" => SwapMode::Manual,
        _ => SwapMode::Auto,
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
    let swap_mode = get_swap_mode(&config);

    // Determine effective mode
    let effective_mode = match swap_mode {
        SwapMode::Auto => {
            let swapfc_path = config.get("swapfc_path").unwrap_or("/swapfc/swapfile");
            if is_path_btrfs(swapfc_path) || is_root_btrfs() {
                // Desktop optimization: zswap + swapfc - tested to be faster
                info!("Auto-detected btrfs: using zswap + swapfc");
                SwapMode::ZswapSwapfc
            } else {
                info!("Auto-detected non-btrfs: using zram only");
                SwapMode::ZramOnly
            }
        }
        mode => mode,
    };

    #[allow(unused_assignments)]
    let mut zswap_backup: Option<ZswapBackup> = None;

    match effective_mode {
        SwapMode::ZramSwapfc => {
            // Desktop-optimized mode: zram for speed + swapfc for overflow
            // zram is faster than zswap because it's a dedicated block device
            
            // Set up signal handler
            signal_hook::flag::register(
                signal_hook::consts::SIGTERM,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )?;
            ctrlc::set_handler(move || {
                request_shutdown();
            })?;

            // Start zram first (primary high-priority swap)
            info!("Setting up zram as primary swap...");
            if let Err(e) = systemd_swap::zram::start(&config) {
                error!("Zram: {}", e);
            }

            // Create swapfc for overflow/writeback (lower priority)
            info!("Setting up swapfc as secondary swap for overflow...");
            let mut swapfc = SwapFc::new(&config)?;
            
            // Create initial swap file
            swapfc.create_initial_swap()?;

            // Run swapfc monitoring loop
            swapfc.run()?;
        }

        SwapMode::ZswapSwapfc => {
            // For zswap: create swap file FIRST, then enable zswap
            // zswap needs a backing swap device to work
            
            // Set up signal handler
            signal_hook::flag::register(
                signal_hook::consts::SIGTERM,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )?;
            ctrlc::set_handler(move || {
                request_shutdown();
            })?;

            // Create initial swap file and start monitoring
            let mut swapfc = SwapFc::new(&config)?;
            
            // Force create first swap chunk immediately (zswap needs backing swap)
            info!("Creating initial swap file for zswap backing...");
            swapfc.create_initial_swap()?;

            // Now configure zswap (after swap is available)
            match systemd_swap::zswap::start(&config) {
                Ok(backup) => {
                    zswap_backup = Some(backup);
                    save_zswap_backup(&zswap_backup)?;
                }
                Err(e) => error!("Zswap: {}", e),
            }

            // Run swapfc monitoring loop
            swapfc.run()?;
        }

        SwapMode::ZramOnly => {
            // For zram: just set up zram, no swap files needed
            if let Err(e) = systemd_swap::zram::start(&config) {
                error!("Zram: {}", e);
            }
            notify_ready();
            info!("Zram setup complete");
            
            // Keep running to respond to signals
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                if systemd_swap::is_shutdown() {
                    break;
                }
            }
        }

        SwapMode::Manual => {
            // Legacy mode: use explicit config values
            
            // Warn about incompatible configurations
            if config.get_bool("zram_enabled")
                && (config.get_bool("zswap_enabled") || config.get_bool("swapfc_enabled"))
            {
                warn!("Combining zram with zswap/swapfc can lead to LRU inversion");
            }

            // Start zswap if enabled
            if config.get_bool("zswap_enabled") {
                match systemd_swap::zswap::start(&config) {
                    Ok(backup) => {
                        zswap_backup = Some(backup);
                        save_zswap_backup(&zswap_backup)?;
                    }
                    Err(e) => error!("Zswap: {}", e),
                }
            }

            // Start zram if enabled
            if config.get_bool("zram_enabled") {
                if let Err(e) = systemd_swap::zram::start(&config) {
                    error!("Zram: {}", e);
                }
            }

            // Start swapfc if enabled
            if config.get_bool("swapfc_enabled") {
                signal_hook::flag::register(
                    signal_hook::consts::SIGTERM,
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                )?;
                ctrlc::set_handler(move || {
                    request_shutdown();
                })?;

                let mut swapfc = SwapFc::new(&config)?;
                swapfc.run()?;
            } else {
                notify_ready();
                info!("Swap setup complete");
            }
        }

        SwapMode::Auto => unreachable!("Auto mode should be resolved before this point"),
    }

    Ok(())
}

/// Save zswap backup for later restoration
fn save_zswap_backup(backup: &Option<ZswapBackup>) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(ref backup) = backup {
        let backup_path = format!("{}/zswap_backup", WORK_DIR);
        makedirs(&backup_path)?;
        for (path, value) in &backup.parameters {
            let filename = Path::new(path).file_name().unwrap_or_default();
            let save_path = format!("{}/{}", backup_path, filename.to_string_lossy());
            fs::write(&save_path, format!("{}={}", path, value))?;
        }
    }
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
    for subsystem in ["swapfc", "zram"] {
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
    let is_root = am_i_root().is_ok();
    if !is_root {
        warn!("Not root! Some output might be missing.");
    }

    let swap_stats = get_mem_stats(&["MemTotal", "SwapTotal", "SwapFree"])?;
    let _mem_total = swap_stats["MemTotal"];
    let swap_total = swap_stats["SwapTotal"];
    let swap_used = swap_total - swap_stats["SwapFree"];

    // Zswap status
    if let Some(zswap) = systemd_swap::zswap::get_status() {
        println!("Zswap:");
        println!("  enabled: {}", zswap.enabled);
        println!("  compressor: {}", zswap.compressor);
        println!("  zpool: {}", zswap.zpool);
        println!("  max_pool_percent: {}%", zswap.max_pool_percent);

        // Try to get basic stats from /proc/meminfo (works without root!)
        if let Ok(usage) = systemd_swap::meminfo::get_effective_swap_usage() {
            if usage.zswap_active {
                let zswap_original = swap_used.saturating_sub(usage.swap_used_disk);
                let zswap_compressed = usage.zswap_pool_bytes;
                
                let ratio = if zswap_original > 0 {
                    (zswap_compressed as f64 / zswap_original as f64) * 100.0
                } else {
                    0.0
                };

                println!();
                println!("  === Pool Statistics ===");
                println!("  pool_size: {:.1} MiB (compressed)", zswap_compressed as f64 / 1024.0 / 1024.0);
                println!("  stored_data: {:.1} MiB (original)", zswap_original as f64 / 1024.0 / 1024.0);
                println!("  pool_utilization: {}%", usage.zswap_pool_percent);
                println!("  compress_ratio: {:.0}%", ratio);

                // If running as root, show additional debugfs stats
                if is_root && (zswap.stored_pages > 0 || zswap.written_back_pages > 0) {
                    let page_size = get_page_size();
                    
                    println!();
                    println!("  === Writeback Statistics (debugfs) ===");
                    println!("  stored_pages: {}", zswap.stored_pages);
                    println!("  same_filled_pages: {}", zswap.same_filled_pages);
                    println!("  written_back_pages: {} ({:.1} MiB)", 
                             zswap.written_back_pages, 
                             (zswap.written_back_pages * page_size) as f64 / 1024.0 / 1024.0);
                    println!("  pool_limit_hit: {}", zswap.pool_limit_hit);
                    println!("  reject_reclaim_fail: {}", zswap.reject_reclaim_fail);
                }

                // Show effective swap usage
                if swap_used > 0 {
                    println!();
                    println!("  === Effective Swap Usage ===");
                    println!("  kernel_reported_used: {:.1} MiB", swap_used as f64 / 1024.0 / 1024.0);
                    println!("  in_zswap_pool (RAM): {:.1} MiB", zswap_original as f64 / 1024.0 / 1024.0);
                    println!("  actual_disk_used: {:.1} MiB", usage.swap_used_disk as f64 / 1024.0 / 1024.0);
                    let percent_in_ram = (zswap_original as f64 / swap_used as f64) * 100.0;
                    println!("  swap_in_ram: {:.0}%", percent_in_ram);
                }
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

