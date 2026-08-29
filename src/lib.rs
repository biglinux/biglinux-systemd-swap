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

/// Marker `systemd-swap stop` leaves while it tears swap down.
///
/// Deliberately outside the work directory, which stop() removes partway
/// through its own teardown. A marker in there would be deleted while swap
/// files are still being removed, reopening the window it exists to close.
pub const STOPPING_MARKER: &str = "/run/systemd-swap.stopping";

/// Whether swap is going away, either because this process was signalled or
/// because a stop is running alongside it.
///
/// systemd runs ExecStop as its own process and only signals the daemon once
/// that has finished, so for the whole teardown the daemon is still monitoring
/// while its devices are removed underneath it. The shutdown flag is never set
/// in that window, which is why the marker is needed as well.
pub fn stop_in_progress() -> bool {
    is_shutdown() || std::path::Path::new(STOPPING_MARKER).exists()
}
