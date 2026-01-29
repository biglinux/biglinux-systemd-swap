// Configuration parsing for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

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

        // Set environment variables for config expansion
        let ncpu = crate::meminfo::get_cpu_count();
        let ram_size = crate::meminfo::get_ram_size().unwrap_or(0);
        env::set_var("NCPU", ncpu.to_string());
        env::set_var("RAM_SIZE", ram_size.to_string());

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
                                config_files
                                    .insert(basename.to_string_lossy().to_string(), path_str.to_string());
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

    /// Apply optimized values from autoconfig (only if not explicitly set)
    /// This allows hardware-based auto-tuning while respecting user overrides
    /// Apply optimized values from autoconfig
    /// Currently this only logs the detection result, as specific parameters are static
    pub fn apply_autoconfig(&mut self, _recommended: &crate::autoconfig::RecommendedConfig) {
        // We no longer override specific parameters dynamically.
        // The mode selection happens in main.rs based on recommendations.
        debug!("Autoconfig applied (mode selection only)");
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
                // Expand shell variables using sh -c "echo ..."
                let expanded = Command::new("sh")
                    .arg("-c")
                    .arg(format!("echo {}", value))
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|_| value.to_string());

                config.insert(key.to_string(), expanded);
            }
        }

        Ok(config)
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

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_simple_config() {
        // This would need a temp file for proper testing
    }
}
