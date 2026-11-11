//! Shutdown ends a monitor's wait at once. A binary of its own: the shutdown
//! flag is process-global, and once set it stays set.

use std::time::{Duration, Instant};
use systemd_swap::meminfo::PressureWait;

#[test]
fn shutdown_ends_a_long_wait() {
    let pressure = PressureWait::new();
    let start = Instant::now();
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        systemd_swap::request_shutdown();
    });
    // Memory pressure may end a wait early; shutdown must end the last one.
    while !systemd_swap::is_shutdown() {
        pressure.wait(Duration::from_secs(60));
    }
    assert!(start.elapsed() < Duration::from_secs(5));

    let start = Instant::now();
    assert!(!pressure.wait(Duration::from_secs(60)));
    assert!(start.elapsed() < Duration::from_secs(1));
}
