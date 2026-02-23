//! Configuration parsing for systemd-swap.
//!
//! Reads key=value config files and expands shell-style `${VAR}` references.
//! Arithmetic expressions of the form `a OP b` (where OP is +, -, *, /) are
//! also evaluated at parse time.
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
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

        // Inject system-derived values without unsafe env::set_var.
        // expand_value uses this map before falling back to std::env::vars().
        let mut system_vars: HashMap<String, String> = HashMap::new();
        system_vars.insert(
            "NCPU".to_string(),
            crate::meminfo::get_cpu_count().to_string(),
        );
        system_vars.insert(
            "RAM_SIZE".to_string(),
            crate::meminfo::get_ram_size().unwrap_or(0).to_string(),
        );

        // Load default config
        if Path::new(DEF_CONFIG).exists() {
            if let Ok(cfg) = Self::parse_config(DEF_CONFIG, &system_vars) {
                values.extend(cfg);
            }
        }

        // Load /etc/systemd/swap.conf
        if Path::new(ETC_CONFIG).exists() {
            match Self::parse_config(ETC_CONFIG, &system_vars) {
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
            if let Ok(cfg) = Self::parse_config(&path, &system_vars) {
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
    ///
    /// Only called in auto mode. For explicit modes, each subsystem uses
    /// its own fallback defaults from `unwrap_or()` calls.
    pub fn apply_autoconfig(
        &mut self,
        recommended: &crate::autoconfig::RecommendedConfig,
    ) {
        info!("Autoconfig: applying recommended configuration for detected hardware");

        for (key, value) in recommended.config_pairs() {
            self.set_if_missing(key, &value);
        }

        info!("Autoconfig: injection complete");
    }

    /// Parse a single config file
    fn parse_config<P: AsRef<Path>>(
        path: P,
        extra_vars: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        let mut config = HashMap::new();
        let content = fs::read_to_string(path)?;

        for line in content.lines() {
            let line = line.trim();

            // Skip comments and empty lines
            if line.starts_with('#') || !line.contains('=') {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                // Strip inline comments (everything from the first unquoted '#')
                let value = value
                    .split_once('#')
                    .map(|(v, _)| v)
                    .unwrap_or(value)
                    .trim();
                let expanded = Self::expand_value(value, extra_vars);
                config.insert(key.to_string(), expanded);
            }
        }

        Ok(config)
    }

    /// Safely expand environment variables and simple arithmetic in config values
    /// without invoking a shell.
    fn expand_value(value: &str, extra_vars: &HashMap<String, String>) -> String {
        let mut result = value.to_string();
        // Expand injected system variables first (NCPU, RAM_SIZE) — no env mutation needed.
        for (key, val) in extra_vars {
            result = result.replace(&format!("${{{}}}", key), val);
            result = result.replace(&format!("${}", key), val);
        }
        // Expand ambient environment variables ${VAR} and $VAR
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

    /// Evaluate basic integer arithmetic: `number OP number` where OP is one of
    /// `+`, `-`, `*`, `/`.
    ///
    /// **Important constraints:**
    /// - Supports only a single binary operation — no operator precedence, no
    ///   parentheses, no chaining (e.g. `2 + 3 * 4` is NOT supported).
    /// - Operands and results are `i64`. Division truncates toward zero.
    /// - Division by zero yields `0`.
    /// - Unrecognisable expressions are returned unchanged.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from_str(s: &str) -> Config {
        let mut values = HashMap::new();
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                values.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Config { values }
    }

    // ── evaluate_simple_arithmetic ────────────────────────────────────────────

    #[test]
    fn arith_addition() {
        assert_eq!(Config::evaluate_simple_arithmetic("2 + 3"), "5");
    }

    #[test]
    fn arith_subtraction() {
        assert_eq!(Config::evaluate_simple_arithmetic("10 - 4"), "6");
    }

    #[test]
    fn arith_multiplication() {
        assert_eq!(Config::evaluate_simple_arithmetic("3 * 7"), "21");
    }

    #[test]
    fn arith_division() {
        assert_eq!(Config::evaluate_simple_arithmetic("20 / 4"), "5");
    }

    #[test]
    fn arith_division_by_zero() {
        assert_eq!(Config::evaluate_simple_arithmetic("5 / 0"), "0");
    }

    #[test]
    fn arith_plain_number() {
        assert_eq!(Config::evaluate_simple_arithmetic("42"), "42");
    }

    #[test]
    fn arith_unknown_expr_passthrough() {
        assert_eq!(
            Config::evaluate_simple_arithmetic("not_a_number"),
            "not_a_number"
        );
    }

    // ── Config::get_bool ─────────────────────────────────────────────────────

    #[test]
    fn get_bool_true_variants() {
        for v in ["yes", "y", "1", "true", "YES", "True"] {
            let cfg = config_from_str(&format!("key={}", v));
            assert!(cfg.get_bool("key"), "'{}' should be true", v);
        }
    }

    #[test]
    fn get_bool_false_variants() {
        for v in ["no", "0", "false", "off"] {
            let cfg = config_from_str(&format!("key={}", v));
            assert!(!cfg.get_bool("key"), "'{}' should be false", v);
        }
    }

    #[test]
    fn get_bool_missing_key_is_false() {
        let cfg = config_from_str("");
        assert!(!cfg.get_bool("nonexistent"));
    }

    // ── Config::get / get_opt / get_as ───────────────────────────────────────

    #[test]
    fn get_existing_key() {
        let cfg = config_from_str("swap_size=512M");
        assert_eq!(cfg.get("swap_size").unwrap(), "512M");
    }

    #[test]
    fn get_missing_key_is_err() {
        let cfg = config_from_str("");
        assert!(cfg.get("missing").is_err());
    }

    #[test]
    fn get_opt_present() {
        let cfg = config_from_str("foo=bar");
        assert_eq!(cfg.get_opt("foo"), Some("bar"));
    }

    #[test]
    fn get_opt_absent() {
        let cfg = config_from_str("");
        assert_eq!(cfg.get_opt("missing"), None);
    }

    #[test]
    fn get_as_integer() {
        let cfg = config_from_str("count=7");
        assert_eq!(cfg.get_as::<u32>("count").unwrap(), 7u32);
    }

    #[test]
    fn get_as_parse_error() {
        let cfg = config_from_str("count=notanint");
        assert!(cfg.get_as::<u32>("count").is_err());
    }
}
