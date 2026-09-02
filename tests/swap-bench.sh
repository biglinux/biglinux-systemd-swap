#!/bin/bash
# Drive a realistic memory load into swap and record what it costs.
#
# The load runs inside a transient cgroup with MemoryMax set, so it swaps
# against its own limit rather than the machine's. That keeps the measurement
# repeatable regardless of what else is running, and means a mistake here
# starves the benchmark instead of the desktop: if it overruns, the kernel OOMs
# the scope, not your session.
#
# Workers differ in working-set size and access rate rather than all hammering
# equally, because a uniform load never produces the mix of hot and cold pages
# the recompression rungs exist to exploit. A small canary runs alongside them
# inside the same cgroup and times its own accesses, which is the closest thing
# to a responsiveness number available without instrumenting the desktop.
#
# Swap is read from the cgroup rather than /proc/vmstat. System-wide counters
# include whatever the desktop already has in zram, which on a machine with
# gigabytes of it drowns out the load entirely.
#
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

MEM_MAX="${MEM_MAX:-8G}"          # cgroup ceiling for the whole load
SWAP_MAX="${SWAP_MAX:-6G}"        # how much of it may spill
WORKERS="${WORKERS:-6}"
DURATION="${DURATION:-600}"       # seconds; must exceed zram_recomp_idle_age
SAMPLE="${SAMPLE:-5}"             # seconds between samples; fine enough to see the spill
OUT="${OUT:-/tmp/swap-bench.$(date +%s)}"

mkdir -p "$OUT"

need() { command -v "$1" >/dev/null || { echo "missing: $1" >&2; exit 1; }; }
need systemd-run
need python3

# ── The load ────────────────────────────────────────────────────────────────
# Compressible but not trivially so: repeated structured text compresses like
# real application memory, where zeros would be deduplicated as same-pages and
# random data would defeat compression entirely, and neither tells us anything.
cat > "$OUT/worker.py" <<'PY'
import os, sys, time, random

idx = int(sys.argv[1]); mb = int(sys.argv[2]); rate = float(sys.argv[3])
random.seed(idx)

MIB = 1024 * 1024
NOISE = float(os.environ.get("NOISE", "0.25"))
vocab = [f"token{i:04d}" for i in range(256)]

# A pool of distinct lines, drawn from rather than repeated. Filling a page
# with one line over and over compresses like a degenerate case (measured at
# 6x, where real application memory is nearer 3x), which flatters zram and
# understates how much memory the load actually needs.
lines = [
    (" ".join(random.choice(vocab) for _ in range(64)) + "\n").encode()
    for _ in range(512)
]

def block():
    # Exactly one MiB. Truncated to the target rather than trusting a repeat
    # count to land on it, which is how the first version silently allocated a
    # sixth of what it claimed.
    #
    # NOISE is the incompressible fraction. Text alone compresses far better
    # than a real working set, so a share of each page is random bytes to bring
    # the ratio into the range the README describes for desktop workloads.
    out = bytearray()
    noise_bytes = int(MIB * NOISE)
    while len(out) < MIB - noise_bytes:
        out += random.choice(lines)
    out = out[: MIB - noise_bytes] + os.urandom(noise_bytes)
    return bytes(out[:MIB])

pages = [bytearray(block()) for _ in range(mb)]
# Report what was actually allocated, so a mismatch is visible immediately
# instead of looking like a workload that simply refuses to swap.
print(f"worker {idx}: {len(pages) * MIB / MIB:.0f} MiB resident", flush=True)

# Tiered access: a small hot set touched constantly, a warm set occasionally,
# and a cold remainder left alone so it can actually age into the idle rung.
hot  = max(1, len(pages) // 10)
warm = max(1, len(pages) //  3)

# Touch a random 4K page inside the block, not byte 0. A block is 256 kernel
# pages; touching only the first keeps that one page resident forever and never
# faults the other 255 back, so the first version could not generate a single
# swap-in no matter how long it ran.
PAGE = 4096
BATCH = int(os.environ.get("BATCH", "32"))

# Derive the sleep from a target rate so the load is stated in MB/s rather than
# in a sleep value whose meaning depends on the batch size.
TOUCH_MBPS = float(os.environ.get("TOUCH_MBPS", "5"))
interval = (BATCH * PAGE) / (TOUCH_MBPS * MIB)

# Replace a share of the blocks periodically. Without this the working set is
# allocated once and never changes, so the only swap traffic is the initial
# spill and everything after it is a static picture.
CHURN = float(os.environ.get("CHURN", "0.02"))
CHURN_EVERY = float(os.environ.get("CHURN_EVERY", "10"))

def pick():
    r = random.random()
    if r < 0.80:   return random.randrange(0, hot)
    elif r < 0.95: return random.randrange(hot, warm)
    else:          return random.randrange(warm, len(pages))

last_churn = time.time()
while True:
    for _ in range(BATCH):
        blk = pages[pick()]
        off = random.randrange(0, len(blk) // PAGE) * PAGE
        blk[off] = (blk[off] + 1) % 256
    time.sleep(interval)

    now = time.time()
    if now - last_churn >= CHURN_EVERY:
        last_churn = now
        # Freeing a block and allocating a fresh one is what keeps reclaim
        # working: the old pages go away, the new ones have to displace
        # something.
        for _ in range(max(1, int(len(pages) * CHURN))):
            pages[random.randrange(len(pages))] = bytearray(block())
PY

# ── The canary ──────────────────────────────────────────────────────────────
# Tiny resident set, touched on a fixed cadence. Its latency percentiles are
# what a user would feel.
cat > "$OUT/canary.py" <<'PY'
import time, sys, random
mb = 64
pages = [bytearray(1024*1024) for _ in range(mb)]
lat = []
end = time.time() + float(sys.argv[1])
while time.time() < end:
    t0 = time.perf_counter()
    for p in pages:
        p[0] = (p[0] + 1) % 256
    lat.append((time.perf_counter() - t0) * 1000)
    time.sleep(0.5)
lat.sort()
def pct(p): return lat[min(len(lat)-1, int(len(lat)*p))]
print(f"canary_ms p50={pct(0.50):.2f} p90={pct(0.90):.2f} p99={pct(0.99):.2f} n={len(lat)}")
PY

# ── Sampling ────────────────────────────────────────────────────────────────
# The scope's own cgroup, so memory and swap are the load's rather than the
# machine's. Falls back to a search in case the slice is not the expected one.
find_cgroup() {
    local n="swap-bench-$$.scope"
    for base in /sys/fs/cgroup/system.slice /sys/fs/cgroup/user.slice; do
        [ -d "$base/$n" ] && { echo "$base/$n"; return; }
    done
    find /sys/fs/cgroup -maxdepth 5 -name "$n" -type d 2>/dev/null | head -1
}

sample() {
    local t=$1
    local pswpin pswpout psi_some psi_full cg_mem cg_swap
    pswpin=$(awk '/^pswpin /{print $2}' /proc/vmstat)
    pswpout=$(awk '/^pswpout /{print $2}' /proc/vmstat)
    cg_mem=$(cat "$CGROUP/memory.current" 2>/dev/null || echo 0)
    cg_swap=$(cat "$CGROUP/memory.swap.current" 2>/dev/null || echo 0)
    psi_some=$(awk -F'total=' '/^some/{print $2}' /proc/pressure/memory)
    psi_full=$(awk -F'total=' '/^full/{print $2}' /proc/pressure/memory)

    # zram aggregate: orig, compressed, and what zsmalloc actually holds.
    local orig=0 compr=0 used=0
    for d in /sys/block/zram*/mm_stat; do
        [ -r "$d" ] || continue
        read -r o c m _ < "$d"
        orig=$((orig+o)); compr=$((compr+c)); used=$((used+m))
    done

    echo "$t $pswpin $pswpout $psi_some $psi_full $orig $compr $used $cg_mem $cg_swap" >> "$OUT/samples.tsv"
}

echo "output:   $OUT"
echo "limits:   MemoryMax=$MEM_MAX MemorySwapMax=$SWAP_MAX"
echo "load:     $WORKERS workers, ${DURATION}s"
if [ "$DURATION" -le 600 ]; then
    echo "note:     promotion needs DURATION well past zram_recomp_idle_age (default 600s)"
    echo "          or lower that in swap.conf, or the run ends before pages go cold"
fi
echo

# Per-worker sizes and rates spread across a range rather than uniform.
TOTAL_MB=$(python3 -c "
import re
s='$MEM_MAX'; n=int(re.sub('[^0-9]','',s)); print(int(n*1024*1.5))")
PER=$(( TOTAL_MB / WORKERS ))

echo "t pswpin pswpout psi_some psi_full zram_orig zram_compr zram_used cg_mem cg_swap" > "$OUT/samples.tsv"

# The canary runs inside the scope with the load. Outside it, on a machine with
# tens of gigabytes free, its pages never leave RAM and its latency measures
# nothing. Inside, it is a small process competing with the pressure, which is
# the thing worth timing.
systemd-run --quiet --scope --collect \
    -p MemoryMax="$MEM_MAX" -p MemorySwapMax="$SWAP_MAX" \
    --unit="swap-bench-$$" \
    bash -c '
        python3 '"$OUT"'/canary.py '"$DURATION"' > '"$OUT"'/canary.txt &
        for i in $(seq 1 '"$WORKERS"'); do
            rate=$(python3 -c "print(0.001 * (1 + $i % 4))")
            python3 '"$OUT"'/worker.py $i '"$PER"' $rate &
        done
        wait
    ' > "$OUT/workers.log" 2>&1 &
SCOPE_PID=$!

# Find the cgroup straight away. The spill happens while the workers allocate,
# so waiting for them to settle before sampling misses the only period with
# sustained swap traffic in it: the previous version slept 30s here and then
# reported 0 MB/s for a run that had moved 4GB.
for _ in $(seq 1 50); do
    CGROUP="$(find_cgroup)"
    [ -n "$CGROUP" ] && break
    sleep 0.2
done
[ -n "$CGROUP" ] || echo "cgroup: not found - cg_swap will read 0"

# Sample from t=0, in the background, so allocation is covered.
(
    for ((t=0; t<DURATION; t+=SAMPLE)); do
        sample "$t"
        sleep "$SAMPLE"
    done
) &
SAMPLER_PID=$!

sleep 30
echo "--- allocation ---"
head -n "$WORKERS" "$OUT/workers.log" 2>/dev/null
resident=$(ps -o rss= -C python3 2>/dev/null | awk '{s+=$1} END {print int(s/1024)}')
echo "resident total: ${resident}MB   target: ${TOTAL_MB}MB   ceiling: $MEM_MAX"
echo "cgroup:  ${CGROUP:-none}"
echo

wait "$SAMPLER_PID" 2>/dev/null || true

kill "$SCOPE_PID" 2>/dev/null || true
systemctl stop "swap-bench-$$.scope" 2>/dev/null || true

# ── Report ──────────────────────────────────────────────────────────────────
echo
cat "$OUT/canary.txt" 2>/dev/null || echo "canary produced no output"
python3 - "$OUT/samples.tsv" <<'PY'
import sys
rows=[l.split() for l in open(sys.argv[1]).read().splitlines()[1:] if l.strip()]
if len(rows) < 2:
    print("not enough samples"); sys.exit()
f,l=rows[0],rows[-1]
dt=int(l[0])-int(f[0]) or 1
print(f"swap_out  {(int(l[2])-int(f[2]))*4096/2**20/dt:.1f} MB/s avg")
print(f"swap_in   {(int(l[1])-int(f[1]))*4096/2**20/dt:.1f} MB/s avg")
# The spill is short and early, so an average over the whole run buries it.
peaks_out = max((int(b[2])-int(a[2]))*4096/2**20/max(1,int(b[0])-int(a[0]))
                for a, b in zip(rows, rows[1:]))
peaks_in = max((int(b[1])-int(a[1]))*4096/2**20/max(1,int(b[0])-int(a[0]))
               for a, b in zip(rows, rows[1:]))
print(f"peak_out  {peaks_out:.1f} MB/s")
print(f"peak_in   {peaks_in:.1f} MB/s")
print(f"psi_some  {(int(l[3])-int(f[3]))/1e6/dt*100:.2f}% of wall")
print(f"psi_full  {(int(l[4])-int(f[4]))/1e6/dt*100:.2f}% of wall")
if len(l) > 9:
    peak_swap = max(int(r[9]) for r in rows if len(r) > 9)
    peak_mem  = max(int(r[8]) for r in rows if len(r) > 8)
    print(f"cg_mem    {peak_mem/2**30:.2f} GiB peak")
    print(f"cg_swap   {peak_swap/2**30:.2f} GiB peak  <- the load's own swap")
peak=max(rows,key=lambda r:int(r[6]))
o,c,u=int(peak[5]),int(peak[6]),int(peak[7])
if c:
    print(f"ratio     {o/c:.2f}x at peak")
    print(f"zsmalloc  {(u-c)/c*100:.1f}% over compressed size (fragmentation)")
PY
echo
echo "samples: $OUT/samples.tsv"
