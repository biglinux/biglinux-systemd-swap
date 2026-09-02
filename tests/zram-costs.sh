#!/bin/bash
# Time the zram operations the daemon performs on a schedule.
#
# The promotion interval and the decision to compact after every sweep were
# both argued from an assumed cost that was never measured. These operations
# are synchronous writes to sysfs, so timing them directly answers what the
# benchmark cannot: a benchmark shows the aggregate effect of a cadence, not
# what one cycle costs.
#
# Three separate costs, which scale with different things:
#
#   idle        marks pages older than N seconds. Walks the entry table, so it
#               is proportional to disksize regardless of how full the device is.
#   recompress  walks the table again and recompresses what is marked. The walk
#               scales with disksize, the work with how much is eligible.
#   compact     asks zsmalloc to migrate objects out of partly filled zspages.
#               Scales with fragmentation, not with disksize.
#
# Reports absolute time and time per GB of disksize, so the numbers transfer to
# other pool sizes. Run it on an idle-ish system: these are measured in
# milliseconds and a busy machine will add noise.
#
# Needs root to write the sysfs attributes.
#
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

REPS="${REPS:-3}"
IDLE_AGE="${IDLE_AGE:-600}"

[ "$(id -u)" -eq 0 ] || { echo "must run as root (writes zram sysfs)" >&2; exit 1; }

# Milliseconds for one write, or "-" when the attribute rejects it.
time_write() {
    local path=$1 value=$2 start end
    start=$(date +%s%N)
    if ! echo "$value" > "$path" 2>/dev/null; then
        echo "-"
        return
    fi
    end=$(date +%s%N)
    echo "scale=1; ($end - $start) / 1000000" | bc
}

printf '%-8s %9s %9s %9s %9s %9s %10s\n' \
    DEVICE DISKSIZE DATA IDLE_MS RECOMP_MS COMPACT_MS MS_PER_GB

total_idle=0; total_recomp=0; total_compact=0; total_gb=0

for dev in /sys/block/zram*; do
    [ -r "$dev/disksize" ] || continue
    disksize=$(cat "$dev/disksize")
    [ "$disksize" -gt 0 ] || continue
    name=$(basename "$dev")
    gb=$(echo "scale=2; $disksize / 1073741824" | bc)

    read -r orig _ _ < "$dev/mm_stat"
    data_mb=$(echo "scale=0; $orig / 1048576" | bc)

    # Best of REPS: the first pass on a device that has never been marked does
    # more work than the steady state the daemon actually runs in.
    idle_best=99999; recomp_best=99999; compact_best=99999
    for _ in $(seq 1 "$REPS"); do
        t=$(time_write "$dev/idle" "$IDLE_AGE")
        [ "$t" != "-" ] && idle_best=$(echo "if ($t < $idle_best) $t else $idle_best" | bc)

        t=$(time_write "$dev/recompress" "type=idle")
        [ "$t" != "-" ] && recomp_best=$(echo "if ($t < $recomp_best) $t else $recomp_best" | bc)

        t=$(time_write "$dev/compact" "1")
        [ "$t" != "-" ] && compact_best=$(echo "if ($t < $compact_best) $t else $compact_best" | bc)
    done

    [ "$idle_best" = 99999 ] && idle_best="-"
    [ "$recomp_best" = 99999 ] && recomp_best="-"
    [ "$compact_best" = 99999 ] && compact_best="-"

    per_gb="-"
    if [ "$idle_best" != "-" ] && [ "$recomp_best" != "-" ]; then
        per_gb=$(echo "scale=2; ($idle_best + $recomp_best) / $gb" | bc)
        total_idle=$(echo "$total_idle + $idle_best" | bc)
        total_recomp=$(echo "$total_recomp + $recomp_best" | bc)
        total_gb=$(echo "$total_gb + $gb" | bc)
    fi
    [ "$compact_best" != "-" ] && total_compact=$(echo "$total_compact + $compact_best" | bc)

    printf '%-8s %8sG %8sM %9s %9s %10s %10s\n' \
        "$name" "$gb" "$data_mb" "$idle_best" "$recomp_best" "$compact_best" "$per_gb"
done

echo
if [ "$(echo "$total_gb > 0" | bc)" -eq 1 ]; then
    sweep=$(echo "scale=1; $total_idle + $total_recomp" | bc)
    echo "One full sweep of the pool: ${sweep}ms scan + ${total_compact}ms compaction"
    echo "Pool disksize: ${total_gb}G"
    echo
    # What the cadence actually costs, which is the number the interval default
    # should be argued from rather than an assumption.
    for iv in 30 60 120 300 600; do
        pct=$(echo "scale=3; ($sweep + $total_compact) / ($iv * 1000) * 100" | bc)
        printf '  interval=%-4ss -> %s%% of one core\n' "$iv" "$pct"
    done
    echo
    echo "Interleaved, one device per turn, the per-turn cost is a fraction of"
    echo "the sweep figure while the totals above stay the same."
fi
