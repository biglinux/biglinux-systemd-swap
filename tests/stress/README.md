# Stress tests on real hardware

The daemon's hard cases — a zram pool that grew and then meets data that does
not compress, swap files that have to arrive before the kernel runs out of
swap — only show up under real memory pressure on a real kernel. Unit tests
model the arithmetic; these scripts check what the kernel actually did.

Run them only on a machine set aside for testing. They drive it into swap,
may get processes OOM-killed, and the daemon may be restarted between runs.

## Files

- `load.py KIND:MB [KIND:MB ...]` — allocates the phases in order and keeps
  them, then re-reads every page. `comp` compresses about 4x under zstd,
  `random` does not compress. `comp:3072 random:2560` is the mixed load that
  exposed write errors after pool growth.
- `monitor.sh` — every 2 s: MemAvailable, SwapFree, each zram device
  (`ds` disksize, `orig` stored, `phys` RAM used, `ml` mem_limit, in MB),
  the number of swap files and the running count of `Write-error` lines.
- `run.sh NAME COMMAND...` — runs COMMAND under the monitor and prints the
  write errors and OOM kills it caused, the zram peak, and the daemon's
  expansion, contraction and writeback events.

## Procedure

Install the build under test on the test machine, restart the service, then:

```sh
sudo systemctl restart systemd-swap && sleep 20
./run.sh mixed python3 load.py comp:3072 random:2560
./run.sh compressible python3 load.py comp:6144
./run.sh random python3 load.py random:5120
```

Scale the sizes to the machine: the numbers above are for 3.7 GB of RAM.
What to look at:

- `write errors` must be 0 for pure loads. The mixed re-read is the known
  worst case (20-30 on the 3.7 GB notebook with the 35% reserve).
- `zram peak` shows how much the pool held and in how much RAM.
- An OOM kill during a burst that outruns swap file creation is a known
  limit of the reactive swap file path, not a zram failure.
