// Configuration parsing for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;

use glob::glob;
use thiserror::Error;

use crate::{debug, info, warn};

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Missing key: {0}")]
    MissingKey(String),
    #[error("Parse error for {0}: {1}")]
    ParseError(String, String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// Configuration paths
pub const DEF_CONFIG: &str = "/usr/share/systemd-swap/swap-default.conf";
pub const ETC_CONFIG: &str = "/etc/systemd/swap.conf";
pub const VEN_SYSD: &str = "/usr/lib/systemd";
pub const RUN_SYSD: &str = "/run/systemd";
pub const ETC_SYSD: &str = "/etc/systemd";
pub const WORK_DIR: &str = "/run/systemd/swap";

/// Configuration holder
#[derive(Debug, Clone)]
pub struct Config {
    values: HashMap<String, String>,
}

impl Config {
    /// Load configuration from all sources
    pub fn load() -> Result<Self> {
        let mut values = HashMap::new();

        // Store system info for config value expansion.
        // We avoid env::set_var here because it is unsound in multi-threaded
        // programs (UB in Rust 2024 edition). Instead, inject the values
        // directly into the config map so expand_value can pick them up
        // via std::env::vars() from values set early in process startup.
        //
        // SAFETY: This runs before any threads are spawned (called from
        // main before ctrlc::set_handler / thread::spawn), so set_var is
        // safe at this point in the single-threaded startup phase.
        let ncpu = crate::meminfo::get_cpu_count();
        let ram_size = crate::meminfo::get_ram_size().unwrap_or(0);
        // SAFETY: No threads exist yet during Config::load in start()/stop()/status().
        unsafe {
            env::set_var("NCPU", ncpu.to_string());
            env::set_var("RAM_SIZE", ram_size.to_string());
        }

        // Load default config
        if Path::new(DEF_CONFIG).exists() {
            if let Ok(cfg) = Self::parse_config(DEF_CONFIG) {
                values.extend(cfg);
            }
        }

        // Load /etc/systemd/swap.conf
        if Path::new(ETC_CONFIG).exists() {
            match Self::parse_config(ETC_CONFIG) {
                Ok(cfg) => values.extend(cfg),
                Err(e) => warn!("Could not load {}: {}", ETC_CONFIG, e),
            }
        }

        // Load conf.d fragments (etc > run > lib for same basename)
        let mut config_files: HashMap<String, String> = HashMap::new();
        for base_path in [VEN_SYSD, RUN_SYSD, ETC_SYSD] {
            let pattern = format!("{}/swap.conf.d/*.conf", base_path);
            if let Ok(entries) = glob(&pattern) {
                for entry in entries.flatten() {
                    if entry.is_file() {
                        if let Some(basename) = entry.file_name() {
                            if let Some(path_str) = entry.to_str() {
                                debug!("Found {}", path_str);
                                config_files.insert(
                                    basename.to_string_lossy().to_string(),
                                    path_str.to_string(),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Sort by basename and load in order
        let mut sorted_files: Vec<_> = config_files.into_iter().collect();
        sorted_files.sort_by(|a, b| a.0.cmp(&b.0));

        for (_, path) in sorted_files {
            info!("Load: {}", path);
            if let Ok(cfg) = Self::parse_config(&path) {
                values.extend(cfg);
            }
        }

        Ok(Self { values })
    }

    /// Helper: set a config key only if the user hasn't explicitly set it
    fn set_if_missing(&mut self, key: &str, value: &str) {
        if !self.values.contains_key(key) {
            debug!("Autoconfig: injecting {}={}", key, value);
            self.values.insert(key.to_string(), value.to_string());
        } else {
            debug!(
                "Autoconfig: keeping user-defined {}={}",
                key, self.values[key]
            );
        }
    }

    /// Apply optimized values from autoconfig (only if not explicitly set).
    /// This allows hardware-based auto-tuning while respecting user overrides.
    /// When swap_mode=auto, the GUI comments out all keys, so this method
    /// effectively sets all recommended values for the detected hardware.
    pub fn apply_autoconfig(&mut self, recommended: &crate::autoconfig::RecommendedConfig) {
        info!("Autoconfig: applying recommended configuration for detected hardware");

        // Zswap settings
        self.set_if_missing("zswap_compressor", &recommended.zswap_compressor);
        self.set_if_missing(
            "zswap_max_pool_percent",
            &recommended.zswap_max_pool_percent.to_string(),
        );
        self.set_if_missing("zswap_zpool", "zsmalloc");
        self.set_if_missing("zswap_shrinker_enabled", "1");
        self.set_if_missing("zswap_accept_threshold", "85");

        // MGLRU
        self.set_if_missing(
            "mglru_min_ttl_ms",
            &recommended.mglru_min_ttl_ms.to_string(),
        );

        // Zram settings
        self.set_if_missing("zram_alg", &recommended.zram_algorithm);
        self.set_if_missing("zram_size", &format!("{}%", recommended.zram_size_percent));
        self.set_if_missing(
            "zram_mem_limit",
            &format!("{}%", recommended.zram_mem_limit_percent),
        );
        self.set_if_missing("zram_prio", "32767");

        // Swapfile settings
        self.set_if_missing("swapfc_chunk_size", &recommended.swapfc_chunk_size);
        self.set_if_missing("swapfile_chunk_size", &recommended.swapfc_chunk_size);
        self.set_if_missing(
            "swapfc_max_count",
            &recommended.swapfc_max_count.to_string(),
        );
        self.set_if_missing(
            "swapfile_max_count",
            &recommended.swapfc_max_count.to_string(),
        );
        self.set_if_missing(
            "swapfc_free_ram_perc",
            &recommended.swapfc_free_ram_perc.to_string(),
        );
        self.set_if_missing(
            "swapfile_free_ram_perc",
            &recommended.swapfc_free_ram_perc.to_string(),
        );
        self.set_if_missing(
            "swapfc_free_swap_perc",
            &recommended.swapfc_free_swap_perc.to_string(),
        );
        self.set_if_missing(
            "swapfile_free_swap_perc",
            &recommended.swapfc_free_swap_perc.to_string(),
        );
        self.set_if_missing(
            "swapfc_remove_free_swap_perc",
            &recommended.swapfc_remove_free_swap_perc.to_string(),
        );
        self.set_if_missing(
            "swapfile_remove_free_swap_perc",
            &recommended.swapfc_remove_free_swap_perc.to_string(),
        );

        if recommended.swapfc_directio {
            self.set_if_missing("swapfc_directio", "1");
            self.set_if_missing("swapfile_directio", "1");
        }

        info!("Autoconfig: {} values injected", "done");
    }

    /// Check if a key has been explicitly set (vs default)
    pub fn has_explicit(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// Parse a single config file
    fn parse_config<P: AsRef<Path>>(path: P) -> Result<HashMap<String, String>> {
        let mut config = HashMap::new();
        let content = fs::read_to_string(path)?;

        for line in content.lines() {
            let line = line.trim();

            // Skip comments and empty lines
            if line.starts_with('#') || !line.contains('=') {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                let expanded = Self::expand_value(value);
                config.insert(key.to_string(), expanded);
            }
        }

        Ok(config)
    }

    /// Safely expand environment variables and simple arithmetic in config values
    /// without invoking a shell.
    fn expand_value(value: &str) -> String {
        let mut result = value.to_string();
        // Expand environment variables ${VAR} and $VAR
        for (key, val) in std::env::vars() {
            result = result.replace(&format!("${{{}}}", key), &val);
            result = result.replace(&format!("${}", key), &val);
        }
        // Handle simple arithmetic $(( expr )) - only supports basic integer math
        while let Some(start) = result.find("$((") {
            if let Some(end) = result[start..].find("))") {
                let expr = &result[start + 3..start + end];
                let expanded = Self::evaluate_simple_arithmetic(expr);
                result = format!(
                    "{}{}{}",
                    &result[..start],
                    expanded,
                    &result[start + end + 2..]
                );
            } else {
                break;
            }
        }
        result
    }

    /// Evaluate basic integer arithmetic: number OP number where OP is +, -, *, /
    fn evaluate_simple_arithmetic(expr: &str) -> String {
        let expr = expr.trim();
        // Try to parse as a single number first
        if let Ok(n) = expr.parse::<i64>() {
            return n.to_string();
        }
        // Try binary operations
        for op in ['*', '/', '+', '-'] {
            if let Some(pos) = expr.rfind(op) {
                if pos == 0 {
                    continue;
                } // Skip leading minus
                let left = expr[..pos].trim();
                let right = expr[pos + 1..].trim();
                if let (Ok(l), Ok(r)) = (left.parse::<i64>(), right.parse::<i64>()) {
                    let result = match op {
                        '+' => l + r,
                        '-' => l - r,
                        '*' => l * r,
                        '/' => {
                            if r != 0 {
                                l / r
                            } else {
                                0
                            }
                        }
                        _ => unreachable!(),
                    };
                    return result.to_string();
                }
            }
        }
        expr.to_string()
    }

    /// Get a string value
    pub fn get(&self, key: &str) -> Result<&str> {
        self.values
            .get(key)
            .map(|s| s.as_str())
            .ok_or_else(|| ConfigError::MissingKey(key.to_string()))
    }

    /// Get value as specific type
    pub fn get_as<T: std::str::FromStr>(&self, key: &str) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        let value = self.get(key)?;
        value
            .parse()
            .map_err(|e: T::Err| ConfigError::ParseError(key.to_string(), e.to_string()))
    }

    /// Get boolean value (yes/y/1/true = true)
    pub fn get_bool(&self, key: &str) -> bool {
        self.get(key)
            .map(|v| matches!(v.to_lowercase().as_str(), "yes" | "y" | "1" | "true"))
            .unwrap_or(false)
    }

    /// Get optional value
    pub fn get_opt(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|s| s.as_str())
    }
}
