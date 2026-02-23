// systemd-swap - Dynamic swap management for Linux
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};

use systemd_swap::autoconfig::{RecommendedConfig, SwapMode as AutoSwapMode, SystemCapabilities};
use systemd_swap::config::{Config, WORK_DIR};
use systemd_swap::defaults;
use systemd_swap::helpers::{
    am_i_root, find_swap_units, force_remove, get_what_from_swap_unit, makedirs, read_file,
};
use systemd_swap::meminfo::get_mem_stats;
use systemd_swap::swapfile::SwapFile;
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
    /// Show recommended configuration for this system
    Autoconfig,
}

/// Swap strategy based on filesystem detection
#[derive(Debug, Clone, Copy, PartialEq)]
enum SwapMode {
    Auto,
    ZramSwapfc,    // zram + swap files for overflow
    ZswapSwapfc,   // zswap + swapfc (preallocated or sparse loop)
    ZramOnly,      // zram only
    Manual,        // Use explicit config values (zram_enabled, zswap_enabled, swapfc_enabled)
    Disabled,      // Swap management disabled (service exits cleanly)
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Some(Commands::Start) => start(),
        Some(Commands::Stop) => stop(false),
        Some(Commands::Status) => status(),
        Some(Commands::Autoconfig) => autoconfig(),
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

/// Parse swap_mode from config
fn get_swap_mode(config: &Config) -> SwapMode {
    match config
        .get("swap_mode")
        .unwrap_or("auto")
        .to_lowercase()
        .as_str()
    {
        "zram+swapfc" | "zram_swapfc" => SwapMode::ZramSwapfc,
        "zswap+swapfc" | "zswap" | "zswap+swapfile" | "zswap+loopfile" | "zswap_loopfile" => SwapMode::ZswapSwapfc,
        "zram" | "zram_only" => SwapMode::ZramOnly,
        "zram+swapfile" => SwapMode::ZramSwapfc,
        "disabled" => SwapMode::Disabled,
        "manual" => SwapMode::Manual,
        _ => SwapMode::Auto,
    }
}

/// Start a background thread that periodically logs zswap statistics.
/// Useful for observing pool growth and compression ratio.
fn start_zswap_monitor() {
    use std::thread;
    use std::time::Duration;
    use systemd_swap::zswap;

    thread::spawn(move || {
        // Initial delay to let zswap settle
        thread::sleep(Duration::from_secs(10));

        let mut last_wb_pages: u64 = 0;
        let mut last_pool_limit: u64 = 0;

        loop {
            match zswap::get_status() {
                Some(status) => {
                    status.log_summary();

                    // Warn if zswap shrinker is writing back pages rapidly
                    if status.written_back_pages > last_wb_pages + 1000 {
                        info!(
                            "Zswap: shrinker wrote {} pages to disk swap",
                            status.written_back_pages - last_wb_pages
                        );
                    }
                    last_wb_pages = status.written_back_pages;

                    // Warn if pool limit is being hit repeatedly
                    if status.pool_limit_hit > last_pool_limit {
                        warn!(
                            "Zswap: pool limit hit {} more time(s) - consider increasing max_pool_percent",
                            status.pool_limit_hit - last_pool_limit
                        );
                    }
                    last_pool_limit = status.pool_limit_hit;
                }
                None => {
                    warn!("Zswap monitor: failed to read status");
                }
            }

            thread::sleep(Duration::from_secs(30));
        }
    });
}

/// Disable zswap when using zram
/// According to kernel documentation, zswap and zram should not be used together
/// as both perform compression in RAM and can cause:
/// - Double compression (waste of CPU)
/// - LRU inversion issues
/// - Unpredictable memory pressure behavior
fn disable_zswap_for_zram() {
    use systemd_swap::zswap;

    if zswap::is_available() && zswap::is_enabled() {
        info!("Disabling zswap (recommended when using zram)");
        let zswap_enabled = "/sys/module/zswap/parameters/enabled";
        if let Err(e) = std::fs::write(zswap_enabled, "0") {
            warn!("Failed to disable zswap: {}", e);
        } else {
            info!("Zswap disabled successfully");
        }
    }
}



/// Start the swap daemon
fn start() -> Result<(), Box<dyn std::error::Error>> {
    am_i_root()?;

    // Detect system capabilities for autoconfig
    let caps = SystemCapabilities::detect();
    let recommended = RecommendedConfig::from_capabilities(&caps);

    // Clean up any previous instance
    let _ = stop(true);

    // Clean up legacy swapfc/swapfile path
    let legacy_path = Path::new("/swapfc/swapfile");
    if legacy_path.exists() && !legacy_path.is_symlink() {
        info!("Removing legacy path: {}", legacy_path.display());
        if legacy_path.is_dir() {
            let _ = fs::remove_dir_all(legacy_path);
        } else {
            let _ = fs::remove_file(legacy_path);
        }
    }

    // Initialize directories
    makedirs(WORK_DIR)?;
    makedirs(format!(
        "{}/system/local-fs.target.wants",
        systemd_swap::config::RUN_SYSD
    ))?;
    makedirs(format!(
        "{}/system/swap.target.wants",
        systemd_swap::config::RUN_SYSD
    ))?;

    let mut config = Config::load()?;
    let swap_mode = get_swap_mode(&config);

    // Register signal handlers once, before entering any mode
    ctrlc::set_handler(move || {
        request_shutdown();
    })?;

    // Apply autoconfig only in auto mode — for explicit modes, each subsystem
    // uses its own fallback defaults from unwrap_or() calls.
    if matches!(swap_mode, SwapMode::Auto) {
        config.apply_autoconfig(&recommended);
    }

    // Determine effective mode
    let effective_mode = match swap_mode {
        SwapMode::Auto => match recommended.swap_mode {
            AutoSwapMode::ZramSwapfc => {
                info!("Auto-detected: using zram + swapfc");
                SwapMode::ZramSwapfc
            }
            AutoSwapMode::ZramOnly => {
                info!("Auto-detected: using zram only");
                SwapMode::ZramOnly
            }
        },
        mode => mode,
    };

    match effective_mode {
        SwapMode::ZramSwapfc => run_zram_swapfc(&config),
        SwapMode::ZswapSwapfc => run_zswap_swapfc(&config),
        SwapMode::ZramOnly => run_zram_only(&config),
        SwapMode::Manual => run_manual(&config),
        SwapMode::Disabled => {
            info!("Swap management disabled, service will exit");
            notify_ready();
            Ok(())
        }
        SwapMode::Auto => unreachable!("Auto mode should be resolved before this point"),
    }
}

/// ZramSwapfc: zram pool as primary + swapfc as overflow backing
fn run_zram_swapfc(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    // Desktop-optimized mode: zram pool for speed + swapfc for overflow
    // zram is faster than zswap because it's a dedicated block device

    // Disable zswap when using zram (per kernel documentation)
    disable_zswap_for_zram();

    // Start zram pool (primary high-priority swap)
    info!("Setting up ZramPool as primary swap...");
    let zram_ok = match systemd_swap::zram::ZramPool::new(config) {
        Ok(mut pool) => match pool.start_primary() {
            Ok(()) => {
                // Run pool monitor in background thread (handles expansion/contraction)
                std::thread::spawn(move || {
                    if let Err(e) = pool.run_monitor() {
                        warn!("ZramPool monitor error: {}", e);
                    }
                });
                true
            }
            Err(e) => {
                error!("ZramPool: start_primary failed: {}", e);
                false
            }
        },
        Err(e) => {
            error!("ZramPool: init failed: {}", e);
            false
        }
    };

    // Create swapfc for overflow (lower priority) - non-critical
    info!("Setting up swapfc as secondary swap for overflow...");
    match SwapFile::new(config) {
        Ok(mut swapfc) => {
            // Create initial swap file to prevent OOM when zram fills.
            info!("Creating initial swap file for zram overflow protection...");
            if let Err(e) = swapfc.create_initial_swap() {
                warn!(
                    "Initial swapfile creation failed: {} (will retry on demand)",
                    e
                );
            }
            if let Err(e) = swapfc.run() {
                warn!("Swapfile monitor exited: {}", e);
            }
        }
        Err(e) => {
            if zram_ok {
                warn!("Swapfile setup failed, continuing with zram only: {}", e);
                notify_ready();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                    if systemd_swap::is_shutdown() {
                        break;
                    }
                }
            } else {
                error!("Both zram and swapfile failed");
                return Err(e.into());
            }
        }
    }
    Ok(())
}

/// ZswapSwapfc: create swapfile first (zswap needs a backing swap device), then enable zswap
fn run_zswap_swapfc(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    match SwapFile::new(config) {
        Ok(mut swapfc) => {
            swapfc.enable_zswap_mode();
            info!("Creating initial swap file for zswap backing...");
            swapfc.create_initial_swap()?;

            // Now configure zswap (after swap is available) - non-critical
            match systemd_swap::zswap::start(config) {
                Ok(backup) => {
                    let zswap_backup = Some(backup);
                    save_zswap_backup(&zswap_backup)?;
                }
                Err(e) => {
                    warn!("Zswap setup failed, continuing with swapfile only: {}", e);
                }
            }

            start_zswap_monitor();
            swapfc.run()?;
        }
        Err(e) => {
            error!("Swapfile setup failed (required for zswap backing): {}", e);
            return Err(e.into());
        }
    }
    Ok(())
}

/// ZramOnly: zram pool only, no swap files
fn run_zram_only(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    disable_zswap_for_zram();

    match systemd_swap::zram::ZramPool::new(config) {
        Ok(mut pool) => {
            if let Err(e) = pool.start_primary() {
                error!("ZramPool: {}", e);
            }
            notify_ready();
            info!("ZramPool setup complete");

            if let Err(e) = pool.run_monitor() {
                warn!("ZramPool monitor error: {}", e);
            }
        }
        Err(e) => {
            error!("ZramPool: {}", e);
            notify_ready();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                if systemd_swap::is_shutdown() {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Manual mode: legacy mode driven by explicit config flags
fn run_manual(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    warn!("Manual mode: using explicit config flags (zram_enabled, zswap_enabled, swapfc_enabled)");

    if config.get_bool("zswap_enabled") {
        match systemd_swap::zswap::start(config) {
            Ok(backup) => {
                let zswap_backup = Some(backup);
                save_zswap_backup(&zswap_backup)?;
            }
            Err(e) => error!("Zswap: {}", e),
        }
    }

    if config.get_bool("zram_enabled") {
        if !config.get_bool("zswap_enabled") {
            disable_zswap_for_zram();
        }
        if let Err(e) = systemd_swap::zram::start(config) {
            error!("Zram: {}", e);
        }
    }

    if config.get_bool("swapfile_enabled") {
        let mut swapfc = SwapFile::new(config)?;
        swapfc.create_initial_swap()?;
        swapfc.run()?;
    } else {
        notify_ready();
        info!("Manual mode swap setup complete");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
            if systemd_swap::is_shutdown() {
                break;
            }
        }
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

    // Stop all managed swap units (check both swapfile and legacy swapfc names).
    // On init (on_init=true), skip swapfile units: adopt_existing_swapfiles() will
    // take ownership of them without swapping them off under memory pressure.
    for subsystem in ["swapfile", "swapfc", "zram"] {
        if on_init {
            // On init, skip ALL subsystems: adopt_existing_swapfiles() will
            // reuse swapfiles, and ZramPool will adopt existing zram devices.
            // Doing swapoff under memory pressure causes OOM on low-RAM systems.
            continue;
        }
        for unit_path in find_swap_units() {
            if let Ok(content) = read_file(&unit_path) {
                if content.to_lowercase().contains(subsystem) {
                    if let Some(dev) = get_what_from_swap_unit(&unit_path) {
                        info!("{}: swapoff {}", subsystem, dev);
                        let _ = swapoff(&dev);
                        force_remove(&unit_path, true);

                        if subsystem == "swapfile" && dev.starts_with("/dev/loop") {
                            // Detach the loop device after swapoff to prevent it from
                            // persisting with a "(deleted)" backing file reference.
                            let _ = std::process::Command::new("losetup")
                                .args(["-d", &dev])
                                .status();
                        } else if subsystem == "swapfile" && Path::new(&dev).is_file() {
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

    // Remove swap files (check both current and legacy paths).
    // Skip during on_init: adopt_existing_swapfiles() will reuse them.
    if !on_init {
        let swapfile_path = config.get("swapfile_path").unwrap_or(defaults::SWAPFILE_PATH);
        info!("Removing files in {}...", swapfile_path);
        if let Ok(entries) = fs::read_dir(swapfile_path) {
            for entry in entries.flatten() {
                force_remove(entry.path(), true);
            }
        }
        // Also clean legacy path
        let legacy_swapfc_path = config.get("swapfc_path").unwrap_or("/swapfc/swapfile");
        if legacy_swapfc_path != swapfile_path {
            if let Ok(entries) = fs::read_dir(legacy_swapfc_path) {
                for entry in entries.flatten() {
                    force_remove(entry.path(), true);
                }
            }
        }
    }

    Ok(())
}

/// Format bytes as human-readable size
fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Show swap status
fn status() -> Result<(), Box<dyn std::error::Error>> {
    let swap_stats = get_mem_stats(&["SwapTotal", "SwapFree"])?;
    let swap_total = swap_stats["SwapTotal"];
    let swap_free = swap_stats["SwapFree"];
    let kernel_swap_used = swap_total.saturating_sub(swap_free);

    // Collect zswap usage once (used in both Zswap and Swap sections)
    let swap_usage = systemd_swap::meminfo::get_effective_swap_usage().ok();

    // --- Zswap ---
    if let Some(zswap) = systemd_swap::zswap::get_status() {
        if zswap.enabled {
            println!("Zswap ({}):", zswap.compressor);
            println!("  Pool limit:    {}% of RAM", zswap.max_pool_percent);
            if let Some(ref usage) = swap_usage {
                if usage.zswap_active {
                    let original = usage.zswapped_original_bytes;
                    let compressed = usage.zswap_pool_bytes;
                    let ratio = if compressed > 0 {
                        original as f64 / compressed as f64
                    } else {
                        0.0
                    };
                    println!("  Stored data:   {} → {} compressed ({:.1}x ratio)",
                        format_size(original), format_size(compressed), ratio);
                    println!("  Pool fill:     {}%", usage.zswap_pool_percent);
                } else {
                    println!("  Pool:          empty");
                }
            }
        }
    }

    // --- Zram ---
    if let Some(stats) = systemd_swap::zram::get_zram_stats() {
        if stats.orig_data_size > 0 {
            println!("\nZram:");
            println!("  Capacity:      {}", format_size(stats.disksize));
            println!("  Stored data:   {} → {} compressed ({:.1}x ratio)",
                format_size(stats.orig_data_size), format_size(stats.mem_used_total),
                stats.compression_ratio());
            println!("  Utilization:   {}%", stats.memory_utilization());
        }
    }

    // Parse swapon for individual file details (needed early for du calculation)
    struct SwapEntry {
        name: String,
        size: u64,
        used: u64,
    }

    let mut files: Vec<SwapEntry> = Vec::new();

    if let Ok(output) = Command::new("swapon")
        .args(["--raw", "--noheadings", "--bytes"])
        .stdout(Stdio::piped())
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 4 {
                let name = fields[0];
                if name.contains("loop") || name.contains("swapfile") || name.starts_with("/swapfile/") {
                    files.push(SwapEntry {
                        name: name.to_string(),
                        size: fields[2].parse().unwrap_or(0),
                        used: fields[3].parse().unwrap_or(0),
                    });
                }
            }
        }
    }

    // Actual disk usage (sparse/NOCOW files: real blocks, not apparent size)
    let disk_used = if !files.is_empty() {
        let swapfile_path = Config::load()
            .ok()
            .and_then(|c| c.get("swapfile_path").ok().map(|s| s.to_string()))
            .unwrap_or_else(|| defaults::SWAPFILE_PATH.to_string());
        Command::new("du")
            .args(["-s", "--block-size=1", &swapfile_path])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
            })
    } else {
        None
    };

    // --- Swap ---
    println!("\nSwap:");
    if swap_total > 0 {
        println!("  Total:         {}", format_size(swap_total));

        // Used = In zswap + Disk usage (when zswap active), else kernel metric
        let zswap_stored = swap_usage.as_ref()
            .filter(|u| u.zswap_active)
            .map(|u| u.zswapped_original_bytes)
            .unwrap_or(0);
        let du_bytes = disk_used.unwrap_or(0);
        let swap_used = if zswap_stored > 0 {
            zswap_stored + du_bytes
        } else {
            kernel_swap_used
        };

        let pct = swap_used as f64 / swap_total as f64 * 100.0;
        println!("  Used:          {} ({:.0}%)", format_size(swap_used), pct);

        // Breakdown: In zswap + On disk
        if let Some(ref usage) = swap_usage {
            if usage.zswap_active && swap_used > 0 {
                println!("  In zswap:      {} (compressed to {} in RAM)",
                    format_size(usage.zswapped_original_bytes),
                    format_size(usage.zswap_pool_bytes));
            }
        }
        if du_bytes > 0 && swap_used > 0 {
            println!("  On disk:       {}", format_size(du_bytes));
        }

        if !files.is_empty() {
            let file_total: u64 = files.iter().map(|f| f.size).sum();
            println!("\n  Swap files:    {} ({} capacity)", files.len(), format_size(file_total));

            // Individual file list
            println!();
            println!("  {:<24} {:>12} {:>12}", "Device", "Size", "Used");
            println!("  {}", "-".repeat(50));
            for f in &files {
                println!("  {:<24} {:>12} {:>12}",
                    f.name, format_size(f.size), format_size(f.used));
            }
        }
    } else {
        println!("  none");
    }

    Ok(())
}

/// Show recommended configuration based on system hardware
fn autoconfig() -> Result<(), Box<dyn std::error::Error>> {
    println!("Detecting system capabilities...\n");

    let caps = SystemCapabilities::detect();
    let recommended = RecommendedConfig::from_capabilities(&caps);

    println!("=== System Information ===");
    println!("Swap path filesystem: {:?}", caps.swap_path_fstype);

    println!("\n=== Recommended Mode ===");
    println!("  swap_mode:  {:?}", recommended.swap_mode);

    println!("\n=== Config Keys (auto mode injects these) ===");
    for (key, value) in recommended.config_pairs() {
        println!("  {:<34} {}", key, value);
    }

    Ok(())
}
