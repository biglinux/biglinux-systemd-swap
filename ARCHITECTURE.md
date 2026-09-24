# systemd-swap — Architecture

## 1. Purpose

Dynamic swap manager for Linux. Single Rust binary (`/usr/bin/systemd-swap`)
launched by a systemd unit at boot. It detects the host (RAM, free disk, root
filesystem, live ISO) and brings up an appropriate swap topology:
**zram pool** (primary high-priority compressed swap), **zswap** (compressed
cache backed by a swap file), and/or **swapfc** (dynamic swap files with
btrfs-NOCOW handling). Consumed by BigLinux at
install time, replacing the legacy bash-based `systemd-swap` and the
`auto-zram-and-swappiness` package.

## 2. Crate layout

Single binary + library crate (`Cargo.toml`). One member, no workspace.

| Path                         | Responsibility                                                                                  |
|------------------------------|-------------------------------------------------------------------------------------------------|
| `src/lib.rs`                 | Public re-exports, `SHUTDOWN` / `DRY_RUN` atomic flags, `#![deny(unsafe_code)]`.                |
| `src/main.rs`                | clap CLI (`start`/`status`/`autoconfig`), mode dispatcher, signal handler, monitors.            |
| `src/config.rs`              | Parses `/etc/systemd/swap.conf` + conf.d fragments, shell-style `${VAR}` + arithmetic expansion.|
| `src/defaults.rs`            | Compile-time defaults (paths, MGLRU TTL, conservative fallbacks).                               |
| `src/autoconfig.rs`          | `SystemCapabilities::detect()` → `RecommendedConfig`; picks `SwapMode` (ZramSwapfc / ZramOnly). |
| `src/meminfo.rs`             | `/proc/meminfo`, `/proc/cpuinfo` readers, RAM/CPU helpers.                                      |
| `src/helpers.rs`             | Root check, `findmnt` fstype cache, `run_cmd_output`, glob over `/run/systemd/**/*.swap`.       |
| `src/systemd.rs`             | `systemctl` wrapper, `systemd-escape` for unit names, `sd-notify` ready/stopping/status.        |
| `src/zram.rs`                | ZramPool state machine: hot_add via sysfs → modprobe fallback, `mkswap`, monitor; grows only by slots RAM can back at 1:1, one priority per device, idle and huge-page writeback to a per-device loop-backed store. |
| `src/zswap.rs`               | Sysfs writes to `/sys/module/zswap/parameters/*`, backup/restore of pre-existing settings.      |
| `src/swapfile.rs`            | swapfc engine: btrfs subvol+NOCOW, preallocated files (`fallocate` on btrfs), `mkswap`/`swapon` cycle. |
| `src/pre-systemd-swap`       | Bash one-shot helper (fstab edit + `vm.min_free_kbytes` + early `swapoff`). Not Rust.           |
| `include/*.service`          | systemd unit files; copied verbatim by PKGBUILD.                                                |
| `include/swap-default.conf`  | Shipped default config installed to both `/usr/share` and `/etc`.                               |
| `man/*`                      | `swap.conf(5)` and `systemd-swap(8)` man pages.                                                 |
| `tests/*.rs`, `tests/stress/` | Unit/integration/CLI/boundaries; manual stress scripts (see §7).                               |
| `tests/fstab_idempotent.bats`| bats test of the pre-systemd-swap shell helper's fstab append idempotency.                      |
| `pkgbuild/PKGBUILD`          | Arch packaging (BigLinux fork).                                                                 |

## 3. State machine + message flow

Daemon (`systemd-swap start`):

```
parse CLI ──► DRY_RUN flag set (`--dry-run` never reaches start(): dry_run_plan())
   │
   ▼
am_i_root() ──► SystemCapabilities::detect()  // RAM, free disk, fstype, live ISO
   │
   ▼
clear_previous_instance()   // restores zswap, clears the work dir; running zram
   │                        // devices and swap files are adopted, not swapped off
   ▼
Config::load()  ◄── /usr/share/systemd-swap/swap-default.conf
                  ◄── /etc/systemd/swap.conf
                  ◄── conf.d fragments  (etc > run > lib precedence)
   │
   ▼
get_swap_mode(config)
   │        ├── Auto       ──► RecommendedConfig → ZramSwapfc | ZramOnly
   │        ├── Manual     ──► honor zram_enabled/zswap_enabled/swapfile_enabled
   │        ├── Disabled   ──► notify_ready, exit clean
   │        └── explicit modes pass through
   ▼
ctrlc::set_handler ──► request_shutdown() (atomic flag)
   │
   ▼
apply_autoconfig (auto mode only) ──► configure_mglru()  // lru_gen/min_ttl_ms
   │
   ▼
effective_mode dispatcher:
   ┌─ ZramSwapfc ─► disable_zswap_for_zram
   │                ZramPool::new → start_primary → thread::spawn(run_monitor)
   │                SwapFile::new → create_initial_swap → run() (blocking loop)
   ├─ ZswapSwapfc ► SwapFile::new(enable_zswap_mode) → create_initial_swap
   │                zswap::start → save_zswap_backup → start_zswap_monitor (thread)
   │                SwapFile::run() (blocking loop)
   ├─ ZramOnly  ──► disable_zswap_for_zram → ZramPool::start_primary
   │                run_monitor on main thread
   └─ Manual    ──► gates each subsystem on its *_enabled flag
   │
   ▼
notify_ready ──► monitor loop polls /proc/meminfo + zram/zswap sysfs
   │             grows/shrinks swap, logs, re-notifies status
   ▼
SIGINT/SIGTERM → SHUTDOWN flag → loops return → process exits
```

Stopping the service only ends the daemon: the unit has no `ExecStop=`, so
systemd sends SIGTERM, the loops return and every zram device and swap file
stays active (INVARIANTS H7.1). The next `start` adopts them; at shutdown
systemd deactivates the generated `.swap` units. Each start runs in a mount
namespace of its own, so sysfs paths the previous instance wrote
(`backing_dev`) read back mount-relative (`/loop3`) to the next one;
`backing_dev_of` names a loop by its node for that reason.

Per-subsystem state machines (`ZramPool`, `SwapFile`, `ZswapBackup`) all
follow the same pattern: an owned config struct, an initialisation method
that brings the kernel resource up, and a `run_monitor` / `run` blocking
loop that watches `/proc/meminfo` and `SHUTDOWN`.

## 4. Async boundaries

No async runtime. Concurrency is `std::thread`:

- `main.rs::start_zswap_monitor` — background thread logging zswap stats every 30 s.
- `main.rs::spawn_zram_pool` — `ZramPool::run_monitor` on a background thread
  (zram+swapfile and manual modes; zram-only runs it on the main thread).
- `helpers.rs` — `FS_TYPE_CACHE` (`OnceLock<Mutex<HashMap<…>>>`) guards the
  findmnt cache shared by background threads.
- `ctrlc` crate installs a signal-handler thread that sets `SHUTDOWN`.
- All inter-thread communication is via the two atomic flags
  (`SHUTDOWN`, `DRY_RUN`); no channels.
- SIGINT/SIGTERM are handled solely by `ctrlc` (the `signal-hook` dep was
  removed); sd-notify (`libsystemd::daemon::notify`) is
  fire-and-forget.

The systemd main loop is external (we are a `Type=simple` service, so
systemd ignores `READY=1` and the sd-notify calls are advisory only);
GTK / tokio / glib are absent.

## 5. External processes spawned

Every `Command::new(...)` site in `src/*.rs`. Argv is passed as **fixed
string literals + a single typed positional** (path or unit name produced by
internal validators); there is no shell interpolation, so an explicit `--`
terminator is not used. Untrusted input never reaches an argv slot directly:
sizes are `u64`, paths are `Path`, unit names are first run through
`systemd-escape`.

| Target           | Site (file)                         | Argv invariant                                                        |
|------------------|-------------------------------------|-----------------------------------------------------------------------|
| `systemctl`      | `systemd.rs`                        | First arg is a hard-coded `SystemctlAction` enum variant.             |
| `systemd-escape` | `systemd.rs`                        | Used to *produce* the safe unit name before further argv use.         |
| `findmnt`        | `helpers.rs`                        | `-n -o FSTYPE --target <path>`; path is `Path::to_string_lossy`.      |
| `mkswap`         | `zram.rs`, `swapfile.rs`            | `mkswap [-L <label>] <path>`; path from the internal allocator.       |
| `systemd-run`    | `systemd.rs`                        | `systemd-run --wait --quiet --collect -- swapon [--priority <i32>] [--discard] <path>`: swapon in the host mount namespace, before `daemon-reload`. |
| `swapon`         | `main.rs`                           | `swapon --raw --noheadings --bytes` (read-only, `status`).             |
| `du`             | `main.rs`                           | `du -b <swapfile-path>` (read-only).                                  |
| `btrfs`          | `swapfile.rs`, `zram.rs`, `autoconfig.rs` | `btrfs subvolume create/show` on managed paths.                 |
| `chattr`         | `swapfile.rs`, `zram.rs`            | `chattr +C` on managed swap and writeback paths (NOCOW for btrfs).    |
| `losetup`        | `zram.rs`                           | `losetup -f --show --direct-io=on <store>` / `-d /dev/loopN` for the zram writeback store. |
| `fallocate`      | `swapfile.rs`                       | `fallocate --length <size-bytes> -- <path>` (btrfs swap files); size is `u64`. |

`swapoff` is not spawned: it is the `libc::swapoff` syscall in
`systemd.rs::swapoff`, the crate's one `unsafe` block (H1).

`--dry-run` is honoured by never entering the mutating paths: `start
--dry-run` runs the read-only `dry_run_plan()`. A few zram leaves (writeback, `mem_limit` writes, orphan sweep) also
check `systemd_swap::is_dry_run()` as a second guard; the rest do not.

## 6. Security posture

- `#![deny(unsafe_code)]` in `lib.rs`; the one `unsafe` block is the
  `libc::swapoff` call in `systemd.rs`, under a local `#[allow]` (INVARIANTS H1).
- Argv: every `Command::new` uses fixed string literals for flags; only
  typed values (paths, parsed `u64` sizes, validated unit names) flow in.
  Shell interpolation is impossible.
- Root check: `helpers::am_i_root()` (Linux UID 0) gates `start`.
- Dry-run: `--dry-run` routes `start` to the read-only `dry_run_plan()`;
  see §5 for what it does and does not guard.
- Config parser deliberately rejects unknown arithmetic and clamps
  computed sizes (see `boundaries.rs` tests).
- Heavy systemd unit hardening — `include/systemd-swap.service`:
  `ProtectSystem=strict`, `CapabilityBoundingSet=CAP_SYS_ADMIN`,
  `SystemCallFilter=@system-service @swap @mount`,
  `ReadWritePaths=/sys /run -/swapfile` (every entry must
  exist when the namespace is built, or the unit exits 226/NAMESPACE before
  ExecStart — hence the `-` on the runtime-created swap directory),
  `PrivateNetwork=yes`, `NoNewPrivileges=yes`. Deliberately *no*
  `DeviceAllow=` — one entry promotes `DevicePolicy=auto` to `strict`, and the
  zram/loop nodes the daemon hot-adds at runtime cannot be allowlisted ahead
  of time (a `block-loop` entry once denied `/dev/zram*` and killed the pool).
- `include/pre-systemd-swap.service` documents *why* certain hardenings
  are off (kernel tunable writes, modprobe pull-in, host mount visibility).
- fstab mutation is confined to `pre-systemd-swap` (bash, one-shot,
  idempotent — see `tests/fstab_idempotent.bats`); the Rust daemon
  *never* writes `/etc/fstab` (managed via transient `/run/systemd/system`
  units only).
- No polkit / sudoers needed; service runs as root under systemd.
- Invariants are enumerated in `INVARIANTS.md` (groups H1–H7); unit-file
  comments (`include/*.service`) and call sites cross-reference them.

## 7. Testing strategy

| Layer                       | File                                | Notes                                                                                       |
|-----------------------------|-------------------------------------|---------------------------------------------------------------------------------------------|
| Unit (in-crate)             | `src/*.rs` `#[cfg(test)]`           | Parsers, sysfs path helpers, size math, autoconfig decision tables.                         |
| Black-box library API       | `tests/integration.rs`              | Uses public surface only (`Config::from_pairs_for_tests`, `parse_size`, autoconfig).        |
| Behavioural / config corpus | `tests/behavior.rs`                 | Writes realistic `swap.conf` fragments via `tempfile`, asserts effective subsystem configs. |
| Numeric boundary            | `tests/boundaries.rs`               | Off-by-one cases on autoconfig thresholds, `parse_size` overflow guards, clamp limits.      |
| CLI smoke                   | `tests/cli.rs`                      | Spawns `CARGO_BIN_EXE_systemd-swap`, asserts exit codes / help output / unprivileged start. |
| Stress on real hardware     | `tests/stress/`                     | Manual, on a test machine only: load phases under a sampler, counts write errors and OOM kills. |
| Shell helper                | `tests/fstab_idempotent.bats`       | bats run of `src/pre-systemd-swap`; asserts repeated invocation is idempotent.              |
| Property / fuzz             | —                                   | Not currently present.                                                                      |
| AT-SPI                      | —                                   | N/A (no GUI).                                                                               |

Run baseline: `cargo test --locked`. Stress procedure: `tests/stress/README.md`.

## 8. Packaging

- Arch / BigLinux: `pkgbuild/PKGBUILD` (+ `pkgbuild.install`). Targets
  `x86_64`, `aarch64`. Bundles:
  - `/usr/bin/systemd-swap` (the Rust binary)
  - `/usr/bin/pre-systemd-swap` (bash one-shot)
  - `/usr/lib/systemd/system/{systemd-swap,pre-systemd-swap}.service`
  - `/usr/share/systemd-swap/swap-default.conf`
  - `/etc/systemd/swap.conf` (marked `backup=`)
  - `/usr/share/man/man{5,8}/{swap.conf.5,systemd-swap.8}`
- Replaces / conflicts: `systemd-swap`, `systemd-swap-git`,
  `auto-zram-and-swappiness`, `systemd-oomd-defaults`.
- No Flatpak (system service, not user-facing).
- No AppStream metainfo (headless daemon).
- No polkit policy (executed by systemd as root).
