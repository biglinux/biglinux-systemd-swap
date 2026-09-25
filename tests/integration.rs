//! Autoconfig → Config → subsystem config, through the public library API.
//!
//! Nothing here touches privileged sysfs/procfs paths or runs external commands.
// SPDX-License-Identifier: GPL-3.0-or-later

use systemd_swap::autoconfig::{RecommendedConfig, SwapMode};
use systemd_swap::config::Config;
use systemd_swap::swapfile::SwapFileConfig;
use systemd_swap::zram::ZramPoolConfig;

// ── Autoconfig → ZramPoolConfig pipeline ─────────────────────────────────────

#[test]
fn auto_injected_config_feeds_zram_pool() {
    let mut cfg = Config::from_pairs_for_tests(std::iter::empty::<(&str, &str)>());
    cfg.apply_autoconfig(&RecommendedConfig::default());
    let pool = ZramPoolConfig::from_config(&cfg);
    assert_eq!(pool.algorithm, "zstd");
    assert_eq!(pool.size_ceiling_percent, 150);
    // The RAM ceiling has to survive the whole autoconfig -> Config -> pool
    // path. It used to be absent from config_pairs(), so the pool fell back to
    // "no limit" and nothing bounded the RAM the zsmalloc pool could hold.
    let ram = systemd_swap::meminfo::get_ram_size().unwrap();
    assert_eq!(pool.mem_limit_bytes, ram * 50 / 100);
}

/// An operator asking for no ceiling still gets none, and one asking for a
/// different ceiling is not overwritten by the auto-mode default.
#[test]
fn explicit_zram_mem_limit_survives_autoconfig() {
    let ram = systemd_swap::meminfo::get_ram_size().unwrap();
    // "4G" is here because an absolute ceiling used to parse as 0, which reads
    // as "no limit" — and the pool now derives its disksize from this number,
    // so a silent 0 means advertising zram_size with nothing backing it.
    for (requested, expected) in [("0%", 0), ("35%", ram * 35 / 100), ("4G", 4 << 30)] {
        let mut configuration = Config::from_pairs_for_tests([("zram_mem_limit", requested)]);
        configuration.apply_autoconfig(&RecommendedConfig::default());
        let pool = ZramPoolConfig::from_config(&configuration);
        assert_eq!(pool.mem_limit_bytes, expected, "requested {}", requested);
    }
}

// ── Autoconfig → SwapFileConfig pipeline ─────────────────────────────────────

/// The swap-file keys reach `SwapFileConfig` only in zram+swapfc mode. The
/// injected values equal `SwapFileConfig`'s own fallbacks, so the proof of flow
/// is that the keys are present in the Config, not the values that come out.
#[test]
fn auto_zram_swapfc_feeds_swapfile_config() {
    let rec = RecommendedConfig {
        swap_mode: SwapMode::ZramSwapfc,
    };
    let mut cfg = Config::from_pairs_for_tests([("swapfile_path", "/swap")]);
    cfg.apply_autoconfig(&rec);
    assert_eq!(cfg.get("swapfile_max_count").unwrap(), "28");
    assert_eq!(cfg.get("swapfile_chunk_size").unwrap(), "512M");
    let sc = SwapFileConfig::from_config(&cfg).unwrap();
    assert_eq!(sc.max_count, 28);
    assert_eq!(sc.chunk_size, 512 * 1024 * 1024);

    let mut cfg = Config::from_pairs_for_tests([("swapfile_path", "/swap")]);
    cfg.apply_autoconfig(&RecommendedConfig::default());
    assert_eq!(cfg.get_opt("swapfile_max_count"), None);
}
