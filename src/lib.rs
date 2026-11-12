//! Core library for systemd-swap: dynamic swap management for Linux.
//!
//! Exposes the public modules for the daemon binary and any future consumers.
// SPDX-License-Identifier: GPL-3.0-or-later

#![deny(unsafe_code)]
pub mod autoconfig;
pub mod config;
pub mod defaults;
pub mod helpers;
pub mod meminfo;
pub mod swapfile;
pub mod systemd;
pub mod zram;
pub mod zswap;

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Global shutdown flag for signal handling
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// A pipe written once on shutdown: its read end turns readable and stays so,
/// which ends every monitor wait at once instead of at its timeout.
/// Close-on-exec, so mkswap, swapon and the rest do not inherit it.
static SHUTDOWN_PIPE: OnceLock<Option<(OwnedFd, OwnedFd)>> = OnceLock::new();

fn shutdown_pipe() -> Option<&'static (OwnedFd, OwnedFd)> {
    use nix::fcntl::OFlag;
    SHUTDOWN_PIPE
        .get_or_init(
            || match nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK) {
                Ok(pipe) => Some(pipe),
                Err(e) => {
                    warn!(
                        "Shutdown pipe unavailable ({}); waits end at their timeout",
                        e
                    );
                    None
                }
            },
        )
        .as_ref()
}

/// Readable once shutdown has been requested.
pub fn shutdown_fd() -> Option<BorrowedFd<'static>> {
    shutdown_pipe().map(|(read, _)| read.as_fd())
}

/// Check if shutdown was requested
pub fn is_shutdown() -> bool {
    SHUTDOWN.load(Ordering::Acquire)
}

/// Request shutdown
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    if let Some((_, write)) = shutdown_pipe() {
        let _ = nix::unistd::write(write, &[1]);
    }
}

// caveman: --dry-run flag. when true, refuse any system-mutating call
// (modprobe, fstab append, swapon/off, sysfs write). plan-only mode.
static DRY_RUN: AtomicBool = AtomicBool::new(false);

/// Return true if --dry-run was set on the CLI.
pub fn is_dry_run() -> bool {
    DRY_RUN.load(Ordering::Acquire)
}

/// Enable dry-run mode (set once during CLI parsing).
pub fn set_dry_run(v: bool) {
    DRY_RUN.store(v, Ordering::Release);
}
