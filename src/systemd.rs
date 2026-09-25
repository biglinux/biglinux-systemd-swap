//! systemd integration for systemd-swap.
//!
//! Wraps `systemctl` subcommand invocations and the sd-notify protocol so the
//! rest of the codebase never shells out to systemd directly.
// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::CString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::config::RUN_SYSD;
use crate::helpers::{makedirs, relative_symlink, run_cmd_output, write_file};
use crate::{info, warn};

/// Typed systemctl sub-commands used by this daemon.
///
/// Using an enum prevents passing invalid action strings and makes call sites
/// self-documenting.
#[derive(Debug, Clone, Copy)]
pub enum SystemctlAction {
    Start,
    Stop,
    DaemonReload,
}

impl SystemctlAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::DaemonReload => "daemon-reload",
        }
    }
}

#[derive(Error, Debug)]
pub enum SystemdError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Helper error: {0}")]
    Helper(#[from] crate::helpers::HelperError),
    #[error("Command failed: {0}")]
    CommandFailed(String),
}

pub type Result<T> = std::result::Result<T, SystemdError>;

/// Notify systemd that we're ready
pub fn notify_ready() {
    let _ = libsystemd::daemon::notify(false, &[libsystemd::daemon::NotifyState::Ready]);
}

/// Notify status message
pub fn notify_status(status: &str) {
    let _ = libsystemd::daemon::notify(
        false,
        &[(libsystemd::daemon::NotifyState::Status(status.to_string()))],
    );
}

/// Run a systemctl sub-command, optionally targeting a unit.
pub fn systemctl(action: SystemctlAction, unit: &str) -> Result<()> {
    let action_str = action.as_str();
    let mut cmd = Command::new("systemctl");
    cmd.stdout(Stdio::null()).stderr(Stdio::null());

    if matches!(action, SystemctlAction::DaemonReload) {
        cmd.arg(action_str);
    } else {
        cmd.arg(action_str).arg(unit);
    }

    let status = cmd.status()?;

    if status.success() {
        Ok(())
    } else {
        Err(SystemdError::CommandFailed(format!(
            "systemctl {} {} failed with {}",
            action_str, unit, status
        )))
    }
}

/// Put a swap device or file to use at once, then hand its unit to systemd.
///
/// `swapon` first: under the memory pressure that makes the pool grow,
/// `daemon-reload` plus `start` took 7 s for a zram device and 14 s for a swap
/// file on the test notebook, and the swap ran out in between (OOM kill).
/// systemd reads swap state from /proc/swaps, so the unit it loads afterwards
/// is already active and `start` has nothing left to do. The flags must match
/// the ones `gen_swap_unit` wrote for the same unit.
///
/// Through `systemd-run`, so swapon runs in the host's mount namespace rather
/// than this unit's. The kernel keeps the path as seen from the namespace that
/// ran swapon, and ours dies with the daemon: after a restart /proc/swaps read
/// `/zram1` and `/1`, systemd invented `zram1.swap` and `1.swap` for them and
/// failed to swap them off at shutdown. 110 ms on the notebook, against 890 ms
/// for the `daemon-reload` it runs ahead of.
pub fn activate_swap(
    what: &str,
    priority: Option<i32>,
    discard: bool,
    unit_name: &str,
) -> Result<()> {
    let priority = priority.map(|p| p.to_string());
    let mut argv = vec![
        "systemd-run",
        "--wait",
        "--quiet",
        "--collect",
        "--",
        "swapon",
    ];
    if let Some(p) = &priority {
        argv.extend(["--priority", p]);
    }
    if discard {
        argv.push("--discard");
    }
    argv.push(what);
    if let Err(e) = run_cmd_output(&argv) {
        warn!("swapon {} failed ({}); leaving it to systemd", what, e);
    }
    systemctl(SystemctlAction::DaemonReload, "")?;
    systemctl(SystemctlAction::Start, unit_name)
}

/// Device type for swap unit
#[derive(Debug, Clone, Copy)]
pub enum DeviceType {
    File,
    Block,
}

impl std::fmt::Display for DeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceType::File => write!(f, "File"),
            DeviceType::Block => write!(f, "Block/Partition"),
        }
    }
}

/// Generate a swap unit file
pub fn gen_swap_unit(
    what: &Path,
    priority: Option<i32>,
    options: Option<&str>,
    tag: &str,
) -> Result<String> {
    let what = fs::canonicalize(what)?;
    let what_str = what.to_string_lossy();

    // Determine device type
    let metadata = fs::metadata(&what)?;
    let device_type = if metadata.permissions().mode() & 0o170000 == 0o060000 {
        DeviceType::Block
    } else {
        DeviceType::File
    };

    // Get unit name using systemd-escape
    let unit_name = Command::new("systemd-escape")
        .args(["-p", "--suffix=swap", &what_str])
        .stdout(Stdio::piped())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;

    let unit_path = format!("{}/system/{}", RUN_SYSD, unit_name);

    // Build unit content
    let mut content = format!(
        r#"[Unit]
Description=Swap {}
Documentation=https://github.com/Nefelim4ag/systemd-swap

# Generated by systemd-swap
# Tag={}

[Swap]
What={}
TimeoutSec=1h
"#,
        device_type, tag, what_str
    );

    if let Some(prio) = priority {
        content.push_str(&format!("Priority={}\n", prio));
    }

    if let Some(opts) = options {
        content.push_str(&format!("Options={}\n", opts));
    }

    write_file(&unit_path, &content)?;

    // Create symlinks
    let wants_dir = format!("{}/system/swap.target.wants", RUN_SYSD);
    makedirs(&wants_dir)?;
    relative_symlink(&unit_path, format!("{}/{}", wants_dir, unit_name))?;

    if matches!(device_type, DeviceType::File) {
        let local_fs_dir = format!("{}/system/local-fs.target.wants", RUN_SYSD);
        makedirs(&local_fs_dir)?;
        relative_symlink(&unit_path, format!("{}/{}", local_fs_dir, unit_name))?;
    }

    info!("Generated swap unit: {}", unit_name);
    Ok(unit_name)
}

/// Disable a swap device using the swapoff(2) syscall directly
pub fn swapoff(device: &str) -> Result<()> {
    let c_path = CString::new(device).map_err(|_| {
        SystemdError::CommandFailed(format!("invalid path for swapoff: {}", device))
    })?;
    // SAFETY: c_path is a valid NUL-terminated C string; swapoff(2) is a documented Linux syscall.
    #[allow(unsafe_code)]
    let ret = unsafe { libc::swapoff(c_path.as_ptr()) };
    if ret == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        Err(SystemdError::CommandFailed(format!(
            "swapoff {} failed: {}",
            device, err
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swapoff_rejects_nul_byte_in_path() {
        let res = swapoff("/path/with\0nul");
        assert!(res.is_err());
    }

    #[test]
    fn swapoff_nonexistent_device_errors() {
        // Unprivileged invocation: must not panic; returns an error.
        let res = swapoff("/nonexistent/device/xyz");
        assert!(res.is_err());
    }
}
