//! Checks on the shipped systemd unit files.
//!
//! These read `include/*.service` at compile time and fail on settings that
//! would stop the unit before a single line of the daemon runs.
// SPDX-License-Identifier: GPL-3.0-or-later

// ── Unit-file sandbox reachability ──────────────────────────────────────────
//
// `ProtectSystem=strict` makes `ReadWritePaths=` boot-critical: systemd builds
// the mount namespace *before* ExecStart, so an entry that does not exist
// aborts the unit with 226/NAMESPACE without running a single line of this
// daemon. Measured on a live host: a stale `/var/lib/systemd-swap` entry that
// nothing in this tree creates or writes left the machine with no swap at all
// and 4637 restart attempts. A path that may legitimately be absent must carry
// systemd's `-` prefix (ignore-if-missing) instead.

#[test]
fn unit_readwritepaths_are_all_reachable() {
    let line = include_str!("../include/systemd-swap.service")
        .lines()
        .find_map(|l| l.strip_prefix("ReadWritePaths="))
        .expect("systemd-swap.service must declare ReadWritePaths");

    for entry in line.split_whitespace() {
        // Created at runtime by pre-systemd-swap.service; its absence only
        // costs disk swap, so it must not fail the whole namespace.
        if let Some(optional) = entry.strip_prefix('-') {
            assert!(
                optional.starts_with('/'),
                "{entry}: ReadWritePaths entries must be absolute"
            );
            continue;
        }
        assert!(
            entry.starts_with('/'),
            "{entry}: ReadWritePaths entries must be absolute"
        );
        assert!(
            std::path::Path::new(entry).exists(),
            "{entry} does not exist, so systemd cannot bind it read-write and \
             the unit dies 226/NAMESPACE before ExecStart. Either drop the \
             entry or mark it optional as -{entry}."
        );
    }
}

// `DevicePolicy=auto` permits every device only while no `DeviceAllow=` exists;
// a single entry promotes it to `strict`. The daemon hot-adds its own zram and
// loop nodes at runtime, so an allowlist can only be incomplete. Measured cost
// of getting this wrong: `DeviceAllow=block-loop rw` denied /dev/zram*, mkswap
// returned EPERM, and the pool never started.
#[test]
fn unit_does_not_narrow_device_policy() {
    for (file, unit) in [
        (
            "systemd-swap.service",
            include_str!("../include/systemd-swap.service"),
        ),
        (
            "pre-systemd-swap.service",
            include_str!("../include/pre-systemd-swap.service"),
        ),
    ] {
        assert!(
            !unit
                .lines()
                .any(|l| l.trim_start().starts_with("DeviceAllow=")),
            "{file}: DeviceAllow= turns DevicePolicy=auto into strict and locks \
             out the zram/loop nodes this daemon creates at runtime"
        );
    }
}
