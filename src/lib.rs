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

use std::sync::atomic::{AtomicBool, Ordering};

/// Global shutdown flag for signal handling
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Check if shutdown was requested
pub fn is_shutdown() -> bool {
    SHUTDOWN.load(Ordering::Acquire)
}

/// Request shutdown
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
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
