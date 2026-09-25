# AGENTS.md — biglinux-systemd-swap

Single Rust binary (`/usr/bin/systemd-swap`) + one bash one-shot helper.
Dynamic swap manager (zram pool / zswap / swapfc) launched by systemd at
boot. No workspace, no async runtime. GPL-3.0-or-later.

## Build / test / lint (real commands)

```sh
cargo build --release            # or: make build  (then make install DESTDIR=…)
cargo test --all-features --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --no-deps --all-features --workspace   # RUSTDOCFLAGS=-D warnings in CI
cargo deny check advisories bans licenses sources
cargo audit -D warnings
cargo machete --with-metadata
bats tests/fstab_idempotent.bats                 # bash helper idempotency
```

CI mirror: `.github/workflows/ci.yml` (jobs: shellcheck, fmt, clippy, test,
deny, audit, machete, doc, complexity, budgets). Cargo.lock is
committed; gate runs with the locked tree. Binary-size budget 10 MiB.
Behaviour under memory pressure is checked by hand on a test machine with
`tests/stress/` (never on a workstation someone is using).

## Module map (src/: 11 Rust files + 1 bash helper)

| File | Responsibility |
|------|----------------|
| `lib.rs` | Re-exports, `SHUTDOWN`/`DRY_RUN` atomics, `#![deny(unsafe_code)]` |
| `main.rs` | clap CLI (`start`/`status`/`autoconfig`), mode dispatch, monitors |
| `config.rs` | `swap.conf` parser, `${VAR}` + `$(( … ))` expansion (no shell) |
| `defaults.rs` | Compile-time defaults (paths, MGLRU TTL, fallbacks) |
| `autoconfig.rs` | Hardware detect → `RecommendedConfig` → `SwapMode` |
| `meminfo.rs` | `/proc/meminfo` + `/proc/cpuinfo` readers |
| `helpers.rs` | Root check, `run_cmd_output` argv helper, findmnt cache, log macros |
| `systemd.rs` | `systemctl` wrapper, `systemd-escape`, `sd-notify`, sole `unsafe` (`libc::swapoff`) |
| `zram.rs` | ZramPool state machine + monitor |
| `zswap.rs` | zswap sysfs writes, backup/restore |
| `swapfile.rs` | swapfc engine (btrfs NOCOW, preallocated files), mkswap/swapon cycle |
| `src/pre-systemd-swap` | Bash one-shot (fstab edit + sysctl + early swapoff). Not Rust. |

Non-src: `include/*.service` (systemd units), `include/swap-default.conf`
(self-documenting config), `man/`, `pkgbuild/`
(Arch/BigLinux), `Makefile` (DESTDIR install).

## Where to edit

- Swap-strategy decisions / thresholds → `autoconfig.rs`, `defaults.rs`.
- Config keys / parser → `config.rs` (+ doc in `include/swap-default.conf`).
- New subprocess call → go through `helpers::run_cmd_output(&[&str])`; never
  `sh -c`. Unit names from paths → `systemd-escape` in `systemd.rs`.
- Unit hardening → `include/systemd-swap.service` (keep H5.1 filters).

## Invariants & docs

- `INVARIANTS.md` — H1–H7 living contract (memory safety, argv, fstab,
  MGLRU, hardening, config parser, shutdown). Cite by number.
- `ARCHITECTURE.md` — module map, state machine, full external-process table.

## Conventions

- English for code, comments and docs. The daemon is not translated:
  gettext was removed after measuring it — 23 of 25 catalogs were empty,
  and the dependency cost an `unsafe` block and a RUSTSEC advisory.
- Reuse before adding deps; deps stay minimal (std first). No GTK/tokio/glib.
- The daemon is `Type=simple`; `sd-notify` is advisory (systemd ignores
  `READY=1`). Keep code and the unit file in agreement.
- `--dry-run` never enters the mutating paths: `start --dry-run` runs the
  read-only `dry_run_plan()`. Keep new
  mutations out of `dry_run_plan()`; a few zram leaves also check
  `is_dry_run()` as a second guard.
