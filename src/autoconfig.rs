// Automatic system detection and configuration for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;

use crate::defaults;
use crate::helpers::{get_fstype, MB, GB};
use crate::meminfo::get_ram_size;
use crate::{debug, info};


/// Full system capabilities
#[derive(Debug, Clone)]
pub struct SystemCapabilities {
    pub swap_path_fstype: Option<String>,
    pub free_disk_space_bytes: u64,
    pub total_ram_bytes: u64,
    pub is_live_system: bool,
    pub cpu_count: usize,
}

impl SystemCapabilities {
    /// Detect system capabilities
    pub fn detect() -> Self {
        let swap_path = "/swapfile";
        let swap_path_fstype = get_fstype(swap_path).or_else(|| get_fstype("/"));
        let total_ram = get_ram_size().unwrap_or(0);
        let free_space = Self::get_free_disk_space(swap_path).unwrap_or(0);

        let is_live = matches!(
            swap_path_fstype.as_deref(),
            Some("tmpfs") | Some("squashfs") | Some("overlay")
        );

        if is_live {
            info!("Autoconfig: Detected LiveCD/Live system - will use zram only");
        }

        info!(
            "Autoconfig: RAM={} MB, FS={:?}",
            total_ram / MB,
            swap_path_fstype
        );

        Self {
            swap_path_fstype,
            free_disk_space_bytes: free_space,
            total_ram_bytes: total_ram,
            is_live_system: is_live,
            cpu_count: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        }
    }

    /// Get free disk space for a path using statvfs
    fn get_free_disk_space(path: &str) -> Option<u64> {
        let check_path = if Path::new(path).exists() {
            path.to_string()
        } else {
            "/".to_string()
        };

        nix::sys::statvfs::statvfs(check_path.as_str())
            .ok()
            .map(|stat| stat.blocks_available() * stat.block_size())
    }
}

/// Swap mode recommendation
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SwapMode {
    ZramOnly,      // zram without disk backing
    ZramSwapfc,    // zram + pre-allocated swapfiles for overflow
}

/// Recommended swap configuration for auto mode.
///
/// All auto-detected values are consolidated here. The `config_pairs()` method
/// is the single source of truth for which config keys are injected.
#[derive(Debug, Clone)]
pub struct RecommendedConfig {
    pub swap_mode: SwapMode,

    // Zram: disksize = 150% RAM, zstd compression, highest priority
    pub zram_size_percent: u32,
    pub zram_algorithm: String,

    // Swapfiles: 512M chunks, up to 28 files, dynamic growth/shrink
    pub swapfc_chunk_size: String,
    pub swapfc_max_count: u32,
    pub swapfc_free_ram_perc: u8,
    pub swapfc_free_swap_perc: u8,
    pub swapfc_remove_free_swap_perc: u8,
}

impl Default for RecommendedConfig {
    fn default() -> Self {
        Self::zram_only()
    }
}

impl RecommendedConfig {
    /// Zram-only config (no disk swap).
    fn zram_only() -> Self {
        Self {
            swap_mode: SwapMode::ZramOnly,
            zram_size_percent: 150,
            zram_algorithm: defaults::ZRAM_ALG.to_string(),
            swapfc_chunk_size: defaults::SWAPFILE_CHUNK_SIZE.to_string(),
            swapfc_max_count: 0,
            swapfc_free_ram_perc: defaults::SWAPFILE_FREE_RAM_PERC,
            swapfc_free_swap_perc: defaults::SWAPFILE_FREE_SWAP_PERC,
            swapfc_remove_free_swap_perc: defaults::SWAPFILE_REMOVE_FREE_SWAP_PERC,
        }
    }

    /// Zram as primary + pre-allocated swapfiles for overflow.
    ///
    /// Zram handles compression in RAM (150% disksize ≈ 37% RAM at ~4x ratio).
    /// Disk swapfiles provide emergency overflow when zram fills.
    fn zram_swapfc() -> Self {
        Self {
            swap_mode: SwapMode::ZramSwapfc,
            zram_size_percent: 150,
            zram_algorithm: defaults::ZRAM_ALG.to_string(),
            swapfc_chunk_size: defaults::SWAPFILE_CHUNK_SIZE.to_string(),
            swapfc_max_count: defaults::SWAPFILE_MAX_COUNT,
            swapfc_free_ram_perc: defaults::SWAPFILE_FREE_RAM_PERC,
            swapfc_free_swap_perc: defaults::SWAPFILE_FREE_SWAP_PERC,
            swapfc_remove_free_swap_perc: defaults::SWAPFILE_REMOVE_FREE_SWAP_PERC,
        }
    }

    /// Generate recommended configuration based on system capabilities.
    pub fn from_capabilities(caps: &SystemCapabilities) -> Self {
        Self::build_config(caps)
    }

    /// All config key-value pairs that auto mode injects.
    ///
    /// This is the **single source of truth** for auto-mode defaults.
    /// Each subsystem module (zram.rs, swapfile.rs) has its own fallback
    /// defaults in `unwrap_or()` calls, but auto mode overrides them here
    /// for optimal hardware-matched settings.
    pub fn config_pairs(&self) -> Vec<(&str, String)> {
        let mut pairs = vec![
            ("zram_alg", self.zram_algorithm.clone()),
            ("zram_size", format!("{}%", self.zram_size_percent)),
            ("zram_prio", defaults::ZRAM_PRIO.to_string()),
        ];

        if self.swap_mode == SwapMode::ZramSwapfc {
            pairs.extend([
                ("swapfile_chunk_size", self.swapfc_chunk_size.clone()),
                ("swapfile_max_count", self.swapfc_max_count.to_string()),
                ("swapfile_free_ram_perc", self.swapfc_free_ram_perc.to_string()),
                ("swapfile_free_swap_perc", self.swapfc_free_swap_perc.to_string()),
                ("swapfile_remove_free_swap_perc", self.swapfc_remove_free_swap_perc.to_string()),
            ]);
        }

        pairs
    }

    /// Select swap mode: zram+swapfc when disk available, zram-only otherwise.
    ///
    /// Decision logic:
    /// 1. Live system (tmpfs/squashfs/overlay) → zram only
    /// 2. FS doesn't support swapfiles (not btrfs/ext4/xfs) → zram only
    /// 3. Free disk space < total RAM → zram only
    /// 4. Otherwise → zram + pre-allocated swapfiles
    fn build_config(caps: &SystemCapabilities) -> Self {
        if caps.is_live_system {
            debug!("Autoconfig: Live system detected, using zram only");
            return Self::zram_only();
        }

        let supports_swapfiles = caps
            .swap_path_fstype
            .as_deref()
            .map(|fs| matches!(fs, "btrfs" | "ext4" | "xfs"))
            .unwrap_or(false);

        if !supports_swapfiles {
            info!("Autoconfig: FS {:?} does not support swapfiles, using zram only",
                caps.swap_path_fstype);
            return Self::zram_only();
        }

        if caps.free_disk_space_bytes < caps.total_ram_bytes {
            info!("Autoconfig: Not enough disk space (free={:.1}GB < RAM={:.1}GB), using zram only",
                caps.free_disk_space_bytes as f64 / GB as f64,
                caps.total_ram_bytes as f64 / GB as f64);
            return Self::zram_only();
        }

        info!(
            "Autoconfig: using zram + swapfiles (disk {:.1}GB, RAM {:.1}GB, FS={:?})",
            caps.free_disk_space_bytes as f64 / GB as f64,
            caps.total_ram_bytes as f64 / GB as f64,
            caps.swap_path_fstype,
        );
        Self::zram_swapfc()
    }
}
