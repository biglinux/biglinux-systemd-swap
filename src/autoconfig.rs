// Automatic system detection and configuration for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;

use crate::defaults;
use crate::helpers::{get_fstype, GB, MB};
use crate::meminfo::get_ram_size;
use crate::{debug, info};

/// Full system capabilities
#[derive(Debug, Clone)]
pub struct SystemCapabilities {
    pub swap_path_fstype: Option<String>,
    pub free_disk_space_bytes: u64,
    pub total_ram_bytes: u64,
    pub is_live_system: bool,
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
    ZramOnly,   // zram without disk backing
    ZramSwapfc, // zram + pre-allocated swapfiles for overflow
}

/// Recommended swap configuration for auto mode.
///
/// All auto-detected values are consolidated here. The `config_pairs()` method
/// is the single source of truth for which config keys are injected.
#[derive(Debug, Clone)]
pub struct RecommendedConfig {
    pub swap_mode: SwapMode,
}

impl Default for RecommendedConfig {
    fn default() -> Self {
        Self {
            swap_mode: SwapMode::ZramOnly,
        }
    }
}

impl RecommendedConfig {
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
    ///
    /// `zram_size` is virtual capacity, so 150% only costs 150% of RAM if the
    /// data does not compress at all. What the pool may actually hold in RAM is
    /// `zram_mem_limit`, and that is the number that keeps reclaim able to make
    /// progress. Do not re-derive one from the other by assuming a compression
    /// ratio: the ratio falls exactly when the pool fills. Measured on one host
    /// over 21,178 samples, 3.4x while the pool sat below 40% full and 1.93x on
    /// the sample taken 30 s before a reclaim deadlock.
    pub fn config_pairs(&self) -> Vec<(&str, String)> {
        let mut pairs = vec![
            ("zram_alg", defaults::ZRAM_ALG.to_string()),
            ("zram_size", defaults::ZRAM_SIZE.to_string()),
            ("zram_mem_limit", defaults::ZRAM_MEM_LIMIT.to_string()),
            ("zram_prio", defaults::ZRAM_PRIO.to_string()),
        ];

        if self.swap_mode == SwapMode::ZramSwapfc {
            pairs.extend([
                (
                    "swapfile_chunk_size",
                    defaults::SWAPFILE_CHUNK_SIZE.to_string(),
                ),
                (
                    "swapfile_max_count",
                    defaults::SWAPFILE_MAX_COUNT.to_string(),
                ),
                (
                    "swapfile_free_ram_perc",
                    defaults::SWAPFILE_FREE_RAM_PERC.to_string(),
                ),
                (
                    "swapfile_free_swap_perc",
                    defaults::SWAPFILE_FREE_SWAP_PERC.to_string(),
                ),
                (
                    "swapfile_remove_free_swap_perc",
                    defaults::SWAPFILE_REMOVE_FREE_SWAP_PERC.to_string(),
                ),
            ]);
        }

        // MGLRU: always inject so auto mode configures it
        pairs.push(("mglru_min_ttl_ms", defaults::MGLRU_MIN_TTL_MS.to_string()));

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
            return Self::default();
        }

        let supports_swapfiles = caps
            .swap_path_fstype
            .as_deref()
            .map(|fs| matches!(fs, "btrfs" | "ext4" | "xfs"))
            .unwrap_or(false);

        if !supports_swapfiles {
            info!(
                "Autoconfig: FS {:?} does not support swapfiles, using zram only",
                caps.swap_path_fstype
            );
            return Self::default();
        }

        if caps.free_disk_space_bytes < caps.total_ram_bytes {
            info!(
                "Autoconfig: Not enough disk space (free={:.1}GB < RAM={:.1}GB), using zram only",
                caps.free_disk_space_bytes as f64 / GB as f64,
                caps.total_ram_bytes as f64 / GB as f64
            );
            return Self::default();
        }

        info!(
            "Autoconfig: using zram + swapfiles (disk {:.1}GB, RAM {:.1}GB, FS={:?})",
            caps.free_disk_space_bytes as f64 / GB as f64,
            caps.total_ram_bytes as f64 / GB as f64,
            caps.swap_path_fstype,
        );
        Self {
            swap_mode: SwapMode::ZramSwapfc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(fstype: Option<&str>, ram: u64, free: u64, live: bool) -> SystemCapabilities {
        SystemCapabilities {
            swap_path_fstype: fstype.map(String::from),
            free_disk_space_bytes: free,
            total_ram_bytes: ram,
            is_live_system: live,
        }
    }

    // ── build_config decision tree ───────────────────────────────────────────

    #[test]
    fn live_system_uses_zram_only() {
        let c =
            RecommendedConfig::from_capabilities(&caps(Some("overlay"), 8 * GB, 100 * GB, true));
        assert_eq!(c.swap_mode, SwapMode::ZramOnly);
    }

    #[test]
    fn unsupported_fs_uses_zram_only() {
        let c = RecommendedConfig::from_capabilities(&caps(Some("zfs"), 8 * GB, 100 * GB, false));
        assert_eq!(c.swap_mode, SwapMode::ZramOnly);
    }

    #[test]
    fn unknown_fs_uses_zram_only() {
        let c = RecommendedConfig::from_capabilities(&caps(None, 8 * GB, 100 * GB, false));
        assert_eq!(c.swap_mode, SwapMode::ZramOnly);
    }

    #[test]
    fn disk_below_ram_uses_zram_only() {
        // The threshold is `free < RAM`: equal still gets swap files.
        let c = RecommendedConfig::from_capabilities(&caps(Some("btrfs"), 8 * GB, 8 * GB, false));
        assert_eq!(c.swap_mode, SwapMode::ZramSwapfc);
        let c =
            RecommendedConfig::from_capabilities(&caps(Some("btrfs"), 8 * GB, 8 * GB - 1, false));
        assert_eq!(c.swap_mode, SwapMode::ZramOnly);
    }

    #[test]
    fn btrfs_with_enough_disk_uses_zram_swapfc() {
        let c = RecommendedConfig::from_capabilities(&caps(Some("btrfs"), 8 * GB, 100 * GB, false));
        assert_eq!(c.swap_mode, SwapMode::ZramSwapfc);
    }

    #[test]
    fn ext4_with_enough_disk_uses_zram_swapfc() {
        let c = RecommendedConfig::from_capabilities(&caps(Some("ext4"), 4 * GB, 50 * GB, false));
        assert_eq!(c.swap_mode, SwapMode::ZramSwapfc);
    }

    #[test]
    fn xfs_with_enough_disk_uses_zram_swapfc() {
        let c = RecommendedConfig::from_capabilities(&caps(Some("xfs"), 4 * GB, 50 * GB, false));
        assert_eq!(c.swap_mode, SwapMode::ZramSwapfc);
    }

    // ── config_pairs output ──────────────────────────────────────────────────

    #[test]
    fn config_pairs_zram_only_has_no_swapfile_keys() {
        let c = RecommendedConfig::default();
        let pairs = c.config_pairs();
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"zram_alg"));
        assert!(keys.contains(&"zram_size"));
        assert!(keys.contains(&"zram_prio"));
        assert!(keys.contains(&"mglru_min_ttl_ms"));
        assert!(!keys.contains(&"swapfile_chunk_size"));
        assert!(!keys.contains(&"swapfile_max_count"));
    }

    #[test]
    fn config_pairs_zram_swapfc_has_swapfile_keys() {
        let c = RecommendedConfig {
            swap_mode: SwapMode::ZramSwapfc,
        };
        let pairs = c.config_pairs();
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"swapfile_chunk_size"));
        assert!(keys.contains(&"swapfile_max_count"));
        assert!(keys.contains(&"swapfile_free_ram_perc"));
        assert!(keys.contains(&"swapfile_free_swap_perc"));
        assert!(keys.contains(&"swapfile_remove_free_swap_perc"));
    }

    #[test]
    fn config_pairs_zram_size_is_percent_string() {
        let c = RecommendedConfig::default();
        let pairs = c.config_pairs();
        let (_, zram_size) = pairs.iter().find(|(k, _)| *k == "zram_size").unwrap();
        assert!(zram_size.ends_with('%'), "got {}", zram_size);
    }

    #[test]
    fn config_pairs_always_includes_mglru() {
        for c in [
            RecommendedConfig::default(),
            RecommendedConfig {
                swap_mode: SwapMode::ZramSwapfc,
            },
        ] {
            let pairs = c.config_pairs();
            assert!(pairs.iter().any(|(k, _)| *k == "mglru_min_ttl_ms"));
        }
    }

    #[test]
    fn default_is_zram_only() {
        let c = RecommendedConfig::default();
        assert_eq!(c.swap_mode, SwapMode::ZramOnly);
    }
}
