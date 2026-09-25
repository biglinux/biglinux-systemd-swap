//! Deep behavioural tests for the config loader and autoconfig pipeline.
//!
//! Each test writes a realistic config fragment to a temp file, parses it via
//! the same code path that production uses, and checks the effective values
//! flowing into the subsystem configs (`ZramPoolConfig`, `SwapFileConfig`).
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::Write;

use systemd_swap::autoconfig::RecommendedConfig;
use systemd_swap::config::Config;
use systemd_swap::swapfile::SwapFileConfig;
use systemd_swap::zram::ZramPoolConfig;

fn write_conf(content: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    write!(f, "{}", content).unwrap();
    f
}

// ── Comment handling ────────────────────────────────────────────────────────

#[test]
fn strips_full_line_comments() {
    let f = write_conf(
        "# header comment\n\
         zram_alg=zstd\n\
         # trailing comment\n",
    );
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    assert_eq!(cfg.get("zram_alg").unwrap(), "zstd");
}

#[test]
fn strips_inline_comments() {
    let f = write_conf("zram_alg=zstd  # use zstd compressor\nzram_size=150%\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    assert_eq!(cfg.get("zram_alg").unwrap(), "zstd");
    assert_eq!(cfg.get("zram_size").unwrap(), "150%");
}

#[test]
fn ignores_blank_lines() {
    let f = write_conf("\n\n  \nzram_size=100%\n\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    assert_eq!(cfg.get("zram_size").unwrap(), "100%");
}

#[test]
fn ignores_lines_without_equals() {
    let f = write_conf("no equals here\nzram_size=100%\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    assert_eq!(cfg.get("zram_size").unwrap(), "100%");
}

#[test]
fn last_assignment_wins() {
    let f = write_conf("zram_size=100%\nzram_size=200%\nzram_size=300%\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    assert_eq!(cfg.get("zram_size").unwrap(), "300%");
}

// ── Arithmetic expansion ────────────────────────────────────────────────────

#[test]
fn arithmetic_addition_expands() {
    let f = write_conf("answer=$((2 + 3))\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    assert_eq!(cfg.get("answer").unwrap(), "5");
}

// ── Env var expansion ───────────────────────────────────────────────────────

#[test]
fn env_var_expansion_curly() {
    std::env::set_var("SYSTEMD_SWAP_TEST_CURLY", "hello");
    let f = write_conf("greeting=${SYSTEMD_SWAP_TEST_CURLY}\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    assert_eq!(cfg.get("greeting").unwrap(), "hello");
    std::env::remove_var("SYSTEMD_SWAP_TEST_CURLY");
}

#[test]
fn env_var_expansion_bare() {
    std::env::set_var("SYSTEMD_SWAP_TEST_BARE", "world");
    let f = write_conf("greeting=$SYSTEMD_SWAP_TEST_BARE\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    assert_eq!(cfg.get("greeting").unwrap(), "world");
    std::env::remove_var("SYSTEMD_SWAP_TEST_BARE");
}

#[test]
fn undefined_env_var_left_unchanged() {
    let f = write_conf("x=${UNDEFINED_ENV_VAR_XYZ_12345}\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    // No expansion happened; literal is preserved
    assert_eq!(cfg.get("x").unwrap(), "${UNDEFINED_ENV_VAR_XYZ_12345}");
}

// ── End-to-end pipeline: config → subsystem configs ─────────────────────────

#[test]
fn full_pipeline_zram_only_config() {
    let f = write_conf(
        "swap_mode=zram_only\n\
         zram_alg=lz4\n\
         zram_size=100%\n\
         zram_max_devices=4\n",
    );
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    let pool = ZramPoolConfig::from_config(&cfg);
    assert_eq!(pool.algorithm, "lz4");
    assert_eq!(pool.size_ceiling_percent, 100);
    assert_eq!(pool.max_devices, 4);
}

#[test]
fn full_pipeline_swapfile_config() {
    let f = write_conf(
        "swapfile_path=/var/swap\n\
         swapfile_chunk_size=1G\n\
         swapfile_max_count=10\n",
    );
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    let sf = SwapFileConfig::from_config(&cfg).unwrap();
    assert_eq!(sf.path, std::path::PathBuf::from("/var/swap"));
    assert_eq!(sf.chunk_size, 1024 * 1024 * 1024);
    assert_eq!(sf.max_count, 10);
}

#[test]
fn autoconfig_respects_user_override_full_flow() {
    let f = write_conf("zram_size=250%\n");
    let mut cfg = Config::from_file_for_tests(f.path()).unwrap();
    cfg.apply_autoconfig(&RecommendedConfig::default());
    // User value preserved
    assert_eq!(cfg.get("zram_size").unwrap(), "250%");
    // Autoconfig-injected key present
    assert_eq!(cfg.get("zram_alg").unwrap(), "zstd");
    // Flow continues into pool config with preserved override
    let pool = ZramPoolConfig::from_config(&cfg);
    assert_eq!(pool.size_ceiling_percent, 250);
}

// ── Config file precedence via apply_autoconfig ─────────────────────────────

#[test]
fn multiple_apply_autoconfig_calls_are_idempotent() {
    let f = write_conf("zram_alg=lz4\n");
    let mut cfg = Config::from_file_for_tests(f.path()).unwrap();
    let rec = RecommendedConfig::default();
    cfg.apply_autoconfig(&rec);
    let after_first = cfg.get("zram_alg").unwrap().to_string();
    cfg.apply_autoconfig(&rec);
    assert_eq!(cfg.get("zram_alg").unwrap(), after_first);
    // User override still held through repeated autoconfig
    assert_eq!(cfg.get("zram_alg").unwrap(), "lz4");
}

// ── Error paths ─────────────────────────────────────────────────────────────

#[test]
fn config_parse_error_on_invalid_typed_value() {
    let f = write_conf("swapfile_max_count=not_a_number\n");
    let cfg = Config::from_file_for_tests(f.path()).unwrap();
    // The typed accessor fails, so from_config falls back to the default.
    let sf = SwapFileConfig::from_config(&cfg).unwrap();
    assert_eq!(sf.max_count, 28);
}

#[test]
fn from_file_for_tests_missing_file_errors() {
    assert!(Config::from_file_for_tests("/nonexistent/conf.file").is_err());
}
