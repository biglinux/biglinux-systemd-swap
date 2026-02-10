// Memory information parser for /proc/meminfo
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum MemInfoError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Missing field: {0}")]
    MissingField(String),
    #[error("Parse error: {0}")]
    ParseError(String),
}

pub type Result<T> = std::result::Result<T, MemInfoError>;

/// Read memory stats from /proc/meminfo efficiently.
/// Reads only until all requested fields are found, then stops.
pub fn get_mem_stats(fields: &[&str]) -> Result<HashMap<String, u64>> {
    let mut stats = HashMap::new();
    let mut remaining: HashSet<&str> = fields.iter().copied().collect();

    let file = File::open("/proc/meminfo")?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;

        // Parse "Key:   value kB" format
        if let Some(colon_pos) = line.find(':') {
            let key = &line[..colon_pos];

            if remaining.contains(key) {
                let value_part = line[colon_pos + 1..].trim();
                let parts: Vec<&str> = value_part.split_whitespace().collect();

                let value = if parts.len() >= 2 && parts[1] == "kB" {
                    parts[0]
                        .parse::<u64>()
                        .map_err(|e| MemInfoError::ParseError(e.to_string()))?
                        * 1024
                } else if !parts.is_empty() {
                    parts[0]
                        .parse::<u64>()
                        .map_err(|e| MemInfoError::ParseError(e.to_string()))?
                } else {
                    continue;
                };

                stats.insert(key.to_string(), value);
                remaining.remove(key);

                // Early exit if all fields found
                if remaining.is_empty() {
                    break;
                }
            }
        }
    }

    if !remaining.is_empty() {
        return Err(MemInfoError::MissingField(
            remaining.into_iter().collect::<Vec<_>>().join(", "),
        ));
    }

    Ok(stats)
}

/// Get total RAM in bytes
pub fn get_ram_size() -> Result<u64> {
    let stats = get_mem_stats(&["MemTotal"])?;
    Ok(stats["MemTotal"])
}

/// Get free RAM percentage (0-100)
pub fn get_free_ram_percent() -> Result<u8> {
    let stats = get_mem_stats(&["MemTotal", "MemFree"])?;
    let percent = (stats["MemFree"] * 100) / stats["MemTotal"];
    Ok(percent as u8)
}

/// Get free swap percentage (0-100)
pub fn get_free_swap_percent() -> Result<u8> {
    let stats = get_mem_stats(&["SwapTotal", "SwapFree"])?;
    let total = stats["SwapTotal"].max(1); // Prevent divide by zero
    let percent = (stats["SwapFree"] * 100) / total;
    Ok(percent as u8)
}

/// Get page size from system
pub fn get_page_size() -> u64 {
    nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .ok()
        .flatten()
        .unwrap_or(4096) as u64
}

/// Get CPU count
pub fn get_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
}

/// Effective swap usage information accounting for zswap
#[derive(Debug, Default)]
pub struct EffectiveSwapUsage {
    /// Total swap space (bytes)
    pub swap_total: u64,
    /// Free swap space as reported by kernel (bytes)
    pub swap_free: u64,
    /// Swap used as reported by kernel (bytes) - includes zswap cached pages
    pub swap_used_kernel: u64,
    /// Bytes stored in zswap RAM pool (not on disk)
    pub zswap_pool_bytes: u64,
    /// Estimated bytes actually written to disk swap
    pub swap_used_disk: u64,
    /// Zswap pool utilization percentage (0-100)
    pub zswap_pool_percent: u8,
    /// Whether zswap is active and has stored pages
    pub zswap_active: bool,
}

/// Get effective swap usage accounting for zswap compression
/// 
/// When zswap is active, the kernel reports swap usage based on allocated slots,
/// but most of those pages may still be in zswap's RAM pool and not written to disk.
/// This function calculates the actual disk pressure.
/// 
/// Uses /proc/meminfo (Zswap, Zswapped) for basic stats - works without root!
/// Optionally uses debugfs for additional statistics when running as root.
pub fn get_effective_swap_usage() -> Result<EffectiveSwapUsage> {
    // Try to get zswap stats from /proc/meminfo (available without root!)
    // These fields were added in kernel 5.x
    let zswap_fields = get_mem_stats_optional(&["Zswap", "Zswapped"]);
    let (zswap_compressed, zswap_original) = match zswap_fields {
        Ok(fields) => (
            fields.get("Zswap").copied().unwrap_or(0),
            fields.get("Zswapped").copied().unwrap_or(0),
        ),
        Err(_) => (0, 0),
    };

    let stats = get_mem_stats(&["MemTotal", "SwapTotal", "SwapFree"])?;
    let swap_total = stats["SwapTotal"];
    let swap_free = stats["SwapFree"];
    let swap_used_kernel = swap_total.saturating_sub(swap_free);
    let mem_total = stats["MemTotal"];

    let mut result = EffectiveSwapUsage {
        swap_total,
        swap_free,
        swap_used_kernel,
        zswap_pool_bytes: zswap_compressed,
        swap_used_disk: swap_used_kernel.saturating_sub(zswap_original),
        zswap_pool_percent: 0,
        zswap_active: zswap_original > 0 || zswap_compressed > 0,
    };

    // Calculate pool utilization if zswap is active
    if result.zswap_active {
        let max_pool_percent: u64 = std::fs::read_to_string(
            "/sys/module/zswap/parameters/max_pool_percent"
        )
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(20);

        let max_pool_size = mem_total * max_pool_percent / 100;
        if max_pool_size > 0 {
            result.zswap_pool_percent = 
                ((zswap_compressed * 100) / max_pool_size).min(100) as u8;
        }
    }

    Ok(result)
}

/// Read memory stats from /proc/meminfo, ignoring missing fields
fn get_mem_stats_optional(fields: &[&str]) -> Result<HashMap<String, u64>> {
    let mut stats = HashMap::new();
    let mut remaining: HashSet<&str> = fields.iter().copied().collect();

    let file = File::open("/proc/meminfo")?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;

        if let Some(colon_pos) = line.find(':') {
            let key = &line[..colon_pos];

            if remaining.contains(key) {
                let value_part = line[colon_pos + 1..].trim();
                let parts: Vec<&str> = value_part.split_whitespace().collect();

                let value = if parts.len() >= 2 && parts[1] == "kB" {
                    parts[0].parse::<u64>().ok().map(|v| v * 1024)
                } else if !parts.is_empty() {
                    parts[0].parse::<u64>().ok()
                } else {
                    None
                };

                if let Some(v) = value {
                    stats.insert(key.to_string(), v);
                    remaining.remove(key);

                    if remaining.is_empty() {
                        break;
                    }
                }
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_ram_size() {
        let size = get_ram_size().unwrap();
        assert!(size > 0);
    }

    #[test]
    fn test_get_free_ram_percent() {
        let percent = get_free_ram_percent().unwrap();
        assert!(percent <= 100);
    }

    #[test]
    fn test_get_effective_swap_usage() {
        // This test may not work without swap, but should not panic
        let _ = get_effective_swap_usage();
    }
}
