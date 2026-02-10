// Helper utilities for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum HelperError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Command failed: {0}")]
    CommandFailed(String),
    #[error("Not running as root")]
    NotRoot,
}

pub type Result<T> = std::result::Result<T, HelperError>;

/// Check if running as root
pub fn am_i_root() -> Result<()> {
    if nix::unistd::geteuid().is_root() {
        Ok(())
    } else {
        Err(HelperError::NotRoot)
    }
}

/// Read entire file to string
pub fn read_file<P: AsRef<Path>>(path: P) -> Result<String> {
    Ok(fs::read_to_string(path)?)
}

/// Write string to file
/// For sysfs/procfs (virtual filesystems), writes without fsync.
/// For real filesystem paths, calls sync_all to ensure persistence.
pub fn write_file<P: AsRef<Path>>(path: P, content: &str) -> Result<()> {
    let path = path.as_ref();
    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;
    // Skip fsync for virtual filesystems (sysfs, procfs) where it's meaningless
    let path_str = path.to_string_lossy();
    if !path_str.starts_with("/sys/") && !path_str.starts_with("/proc/") {
        file.sync_all()?;
    }
    Ok(())
}

/// Force remove file, ignoring errors
pub fn force_remove<P: AsRef<Path>>(path: P, verbose: bool) {
    let path = path.as_ref();
    match fs::remove_file(path) {
        Ok(()) => {
            if verbose {
                println!("INFO: Removed {}", path.display());
            }
        }
        Err(e) => {
            if verbose {
                eprintln!("WARN: Cannot remove {}: {}", path.display(), e);
            }
        }
    }
}

/// Create directories recursively
pub fn makedirs<P: AsRef<Path>>(path: P) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

/// Create relative symlink
pub fn relative_symlink<P: AsRef<Path>, Q: AsRef<Path>>(target: P, link_name: Q) -> Result<()> {
    let link_name = link_name.as_ref();
    let target = target.as_ref();

    // Remove existing link
    let _ = fs::remove_file(link_name);

    // Calculate relative path
    let link_dir = link_name.parent().unwrap_or(Path::new("."));
    let relative = pathdiff::diff_paths(target, link_dir).unwrap_or(target.to_path_buf());

    symlink(&relative, link_name)?;
    Ok(())
}

/// Run a command and return success status
pub fn run_cmd(cmd: &[&str]) -> Result<bool> {
    let status = Command::new(cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

/// Run a command and capture stdout
pub fn run_cmd_output(cmd: &[&str]) -> Result<String> {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(HelperError::CommandFailed(format!(
            "{} exited with {}",
            cmd[0],
            output.status
        )))
    }
}

/// Find swap unit files in /run/systemd
pub fn find_swap_units() -> Vec<String> {
    let mut units = Vec::new();
    let paths = ["/run/systemd/system", "/run/systemd/generator"];

    for base_path in &paths {
        if let Ok(entries) = glob::glob(&format!("{}/**/*.swap", base_path)) {
            for entry in entries.flatten() {
                if entry.is_file() && !entry.is_symlink() {
                    if let Some(path_str) = entry.to_str() {
                        units.push(path_str.to_string());
                    }
                }
            }
        }
    }
    units
}


/// Get What= value from swap unit file
pub fn get_what_from_swap_unit<P: AsRef<Path>>(path: P) -> Option<String> {
    let content = read_file(path).ok()?;
    for line in content.lines() {
        if let Some(value) = line.strip_prefix("What=") {
            return Some(value.to_string());
        }
    }
    None
}

/// Cache for filesystem type detection (avoids repeated findmnt calls)
static FS_TYPE_CACHE: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

fn fs_cache() -> &'static Mutex<HashMap<PathBuf, String>> {
    FS_TYPE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get the filesystem type of a given path (cached)
pub fn get_fstype<P: AsRef<Path>>(path: P) -> Option<String> {
    let path = path.as_ref();
    // Use parent if path doesn't exist
    let check_path = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|p| p.exists() && *p != Path::new("/"))
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| Path::new("/").to_path_buf())
    };

    // Check cache first
    if let Ok(cache) = fs_cache().lock() {
        if let Some(cached) = cache.get(&check_path) {
            return Some(cached.clone());
        }
    }

    let output = Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "--target", &check_path.to_string_lossy()])
        .stdout(Stdio::piped())
        .output()
        .ok()?;

    let fstype = String::from_utf8_lossy(&output.stdout).trim().to_lowercase();
    if fstype.is_empty() {
        // Fallback to root filesystem
        if check_path != Path::new("/") {
            get_fstype("/")
        } else {
            None
        }
    } else {
        // Store in cache
        if let Ok(mut cache) = fs_cache().lock() {
            cache.insert(check_path, fstype.clone());
        }
        Some(fstype)
    }
}


// Logging macros
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        println!("INFO: {}", format!($($arg)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        eprintln!("WARN: {}", format!($($arg)*))
    };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        eprintln!("ERRO: {}", format!($($arg)*))
    };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if std::env::var("DEBUG").is_ok() {
            eprintln!("DEBUG: {}", format!($($arg)*))
        }
    };
}
