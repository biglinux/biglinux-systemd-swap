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
}
