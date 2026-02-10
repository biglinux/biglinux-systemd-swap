// systemd-swap - Dynamic swap management for Linux
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod autoconfig;
pub mod config;
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
