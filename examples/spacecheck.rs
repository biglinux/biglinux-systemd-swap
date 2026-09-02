//! Report the swap-file allocation decision for a path.
//!
//! Exists so the near-full behaviour can be exercised against real filesystems
//! without running the daemon or touching system swap. It calls the same
//! policy functions has_enough_space() uses, so filling a loopback filesystem
//! and running this reports what the daemon would actually decide.
//!
//!     cargo run --example spacecheck -- <path> [chunk_size] [min_free]
//!
//! Also takes the filesystem numbers directly, so the policy can be reviewed
//! without root or a real filesystem to mount:
//!
//!     cargo run --example spacecheck -- --simulate <total> <free> [unallocated] \
//!         [chunk_size] [min_free]
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;
use std::process::Command;

use systemd_swap::helpers::parse_size;
use systemd_swap::swapfile::{parse_btrfs_unallocated, resolve_min_free};

const BTRFS_MIN_UNALLOCATED: u64 = 1024 * 1024 * 1024;

fn mib(bytes: u64) -> String {
    format!("{:.0}M", bytes as f64 / (1024.0 * 1024.0))
}

fn size_arg(args: &[String], i: usize, default: u64) -> u64 {
    args.get(i)
        .map(|s| parse_size(s).expect("bad size argument"))
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let simulate = args.get(1).map(|s| s == "--simulate").unwrap_or(false);

    let (total, free, unallocated, chunk, configured) = if simulate {
        // Numbers supplied directly: no filesystem, no mount, no root.
        let total = size_arg(&args, 2, 0);
        let free = size_arg(&args, 3, 0);
        let unallocated = args.get(4).filter(|s| *s != "-").map(|s| {
            parse_size(s).expect("bad unallocated")
        });
        (
            total,
            free,
            unallocated,
            size_arg(&args, 5, 512 * 1024 * 1024),
            args.get(6).map(|s| s.as_str()),
        )
    } else {
        let Some(path) = args.get(1) else {
            eprintln!("usage: spacecheck <path> [chunk_size] [min_free]");
            eprintln!("       spacecheck --simulate <total> <free> [unalloc] [chunk] [min_free]");
            std::process::exit(2);
        };

        let stat = nix::sys::statvfs::statvfs(Path::new(path)).expect("statvfs failed");
        let block = stat.block_size();

        // btrfs only: free space says nothing about whether metadata can grow.
        let unallocated = Command::new("btrfs")
            .args(["filesystem", "usage", "-b", path])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| parse_btrfs_unallocated(&String::from_utf8_lossy(&o.stdout)));

        (
            stat.blocks() * block,
            stat.blocks_available() * block,
            unallocated,
            size_arg(&args, 2, 512 * 1024 * 1024),
            args.get(3).map(|s| s.as_str()),
        )
    };

    let reserve = resolve_min_free(total, configured);

    // Mirrors has_enough_space(): the reserve must survive the allocation.
    let space_ok = free >= chunk.saturating_add(reserve);
    let btrfs_ok = unallocated.map(|u| u >= BTRFS_MIN_UNALLOCATED);

    println!(
        "total={} free={} reserve={} chunk={} unallocated={} space_ok={} btrfs_ok={} ALLOW={}",
        mib(total),
        mib(free),
        mib(reserve),
        mib(chunk),
        unallocated.map(mib).unwrap_or_else(|| "-".into()),
        space_ok,
        btrfs_ok.map(|b| b.to_string()).unwrap_or_else(|| "-".into()),
        space_ok && btrfs_ok.unwrap_or(true)
    );
}
