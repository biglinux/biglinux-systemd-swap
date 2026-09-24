//! CLI smoke tests.
//!
//! Spawns the real `systemd-swap` binary via Cargo's `CARGO_BIN_EXE_*` env var
//! and verifies end-user-observable behaviour: exit codes, help output,
//! subcommand availability, and unprivileged-mode graceful failures.
// SPDX-License-Identifier: GPL-3.0-or-later

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_systemd-swap")
}

// ── Help ────────────────────────────────────────────────────────────────────

#[test]
fn help_flag_succeeds() {
    let out = Command::new(bin()).arg("--help").output().unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("systemd-swap"), "stdout was: {}", stdout);
    assert!(stdout.contains("start"));
    // Stopping is the service manager's job: SIGTERM, no swapoff (H7.1).
    assert!(!stdout.contains("stop"));
    assert!(stdout.contains("status"));
    assert!(stdout.contains("autoconfig"));
}

#[test]
fn no_args_prints_help_and_exits_zero() {
    let out = Command::new(bin()).output().unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // main.rs prints help when no subcommand given
    assert!(stdout.contains("systemd-swap"));
}

// ── Read-only subcommands run without root ──────────────────────────────────

#[test]
fn status_runs_without_root() {
    let out = Command::new(bin()).arg("status").output().unwrap();
    assert!(
        out.status.success(),
        "status failed unprivileged.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Must always have a Swap section (even if "none")
    assert!(stdout.contains("Swap:"), "stdout was: {}", stdout);
}

#[test]
fn autoconfig_runs_without_root() {
    let out = Command::new(bin()).arg("autoconfig").output().unwrap();
    assert!(
        out.status.success(),
        "autoconfig failed unprivileged.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for section in [
        "System Information",
        "Recommended Mode",
        "Config Keys",
        "zram_alg",
        "zram_size",
        "mglru_min_ttl_ms",
    ] {
        assert!(stdout.contains(section), "missing {section}: {stdout}");
    }
}

// ── Privileged subcommands exit cleanly when not root ───────────────────────

#[test]
fn start_without_root_exits_non_zero_without_panic() {
    // Checked before the spawn: as root this would start swap on the host
    // and block in the foreground daemon.
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let out = Command::new(bin()).arg("start").output().unwrap();
    assert!(!out.status.success(), "start should fail unprivileged");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "unexpected panic: {}",
        stderr
    );
    // The am_i_root() check surfaces as an ERRO log line
    assert!(
        stderr.contains("ERRO") || stderr.contains("root"),
        "stderr={}",
        stderr
    );
}
