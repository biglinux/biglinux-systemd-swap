#!/bin/bash
# Exercise the swap-file space policy against real filesystems as they fill up.
#
# Builds loopback images of each filesystem type, fills them in steps, and asks
# the daemon's own policy functions (via the spacecheck example) what it would
# decide at each level. Loopback images are used because the interesting states
# are the dangerous ones: a btrfs with no unallocated space left is not
# something to reproduce on a disk anyone cares about.
#
# Images live on a tmpfs by default, sized to the largest case. Filling the
# host filesystem is the exact failure this harness studies, and RAM-backed
# images cannot cause it: the size= limit makes overrun structurally impossible
# rather than something a preflight check has to catch, and it is much faster
# than writing gigabytes to disk.
#
# The cost is RAM, and tmpfs pages are swappable, so a large run can push other
# things into swap. That does not affect what is measured here, which is the
# inner filesystem's own accounting, but it is why the sizing check looks at
# MemAvailable. Set BACKING=disk to use WORKDIR instead, which is the right
# choice for cases too large to hold in memory.
#
# Needs root for losetup and mount. Everything it creates is torn down on exit,
# including on failure.
#
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPACECHECK="$REPO/target/debug/examples/spacecheck"

# Sizes straddle the reserve bands: 2G is below the 1G floor once a 512M chunk
# is added, 8G is where 5% is still under the floor. Larger cases are worth
# running, but only somewhere with the space for them, so they are opt-in.
SIZES_GB="${SIZES_GB:-2 8}"
FSTYPES="${FSTYPES:-btrfs ext4 xfs}"
FILL_STEPS="${FILL_STEPS:-0 50 80 90 95 99}"
BACKING="${BACKING:-tmpfs}"
WORKDIR="${WORKDIR:-/var/tmp}"
CHUNK="${CHUNK:-512M}"

WORK=""
TMPFS_MOUNTED=0
cleanup() {
    set +e
    [ -n "$WORK" ] || return
    for mp in "$WORK"/mnt.*; do
        [ -d "$mp" ] && mountpoint -q "$mp" && umount "$mp"
    done
    for lo in $(losetup -a 2>/dev/null | grep -F "$WORK" | cut -d: -f1); do
        losetup -d "$lo"
    done
    [ "$TMPFS_MOUNTED" -eq 1 ] && umount "$WORK"
    rm -rf "$WORK"
}
trap cleanup EXIT

if [ ! -x "$SPACECHECK" ]; then
    echo "building spacecheck..."
    (cd "$REPO" && cargo build --quiet --example spacecheck)
fi

# ── Without root: sweep the policy instead of real filesystems ───────────────
#
# Mounting a filesystem image needs real CAP_SYS_ADMIN. tmpfs can be mounted in
# an unprivileged user namespace, but loop devices cannot, and btrfs/ext4/xfs
# are not among the types permitted there. So the accounting behaviour of a
# filling filesystem is root-only.
#
# The decision policy is not: feeding the numbers in directly exercises the
# same resolve_min_free the daemon uses. That covers every size and fill level
# cheaply, and is what most reviewing is about. What it cannot show is whether
# a real filesystem reports the numbers we expect at 99% full, which is exactly
# where btrfs stops being predictable.
policy_sweep() {
    echo "Policy sweep (no root: simulated filesystems, real decision logic)"
    echo "Reserve must survive the allocation, so ALLOW needs free >= chunk + reserve."
    echo
    printf '%8s %6s %8s %8s  %s\n' TOTAL FULL FREE RESERVE DECISION
    for total in 2 8 32 100 512 4096; do
        for pct in 50 90 95 99; do
            local free=$(( total * (100 - pct) / 100 ))
            printf '%7sG %5s%% %7sG ' "$total" "$pct" "$free"
            "$SPACECHECK" --simulate "${total}G" "${free}G" - "$CHUNK" \
                | sed -E 's/total=[^ ]* free=[^ ]* //; s/unallocated=- btrfs_ok=- //'
        done
    done
    echo
    echo "btrfs unallocated check (100G filesystem, 20G free, so space alone would allow):"
    for unalloc in 4G 2G 1G 512M 0; do
        printf '  unallocated=%-6s ' "$unalloc"
        "$SPACECHECK" --simulate 100G 20G "$unalloc" "$CHUNK" \
            | sed 's/total=[^ ]* free=[^ ]* reserve=[^ ]* chunk=[^ ]* //'
    done
    echo
    echo "For real filesystem behaviour, run as root:  sudo -E $0"
}

if [ "$(id -u)" -ne 0 ]; then
    policy_sweep
    exit 0
fi

# Images are created and removed one at a time, so the peak is the largest one
# filled to the highest step, plus room for the filesystem's own metadata.
max_size=0
for s in $SIZES_GB; do [ "$s" -gt "$max_size" ] && max_size=$s; done
max_fill=0
for p in $FILL_STEPS; do [ "$p" -gt "$max_fill" ] && max_fill=$p; done

peak=$(( max_size * max_fill / 100 + 1 ))
need=$(( peak + 2 ))

if [ "$BACKING" = tmpfs ]; then
    avail=$(( $(awk '/^MemAvailable:/{print $2}' /proc/meminfo) / 1024 / 1024 ))
    echo "images:   tmpfs, ${need}G limit"
    echo "peak use: ~${peak}G RAM   available: ${avail}G"
    if [ "$avail" -lt $(( need + 2 )) ]; then
        cat >&2 <<EOF

Refusing to run: wants ~${need}G of RAM, only ${avail}G available.

    SIZES_GB="2" sudo -E $0        # smaller cases
    BACKING=disk sudo -E $0        # put the images on WORKDIR instead
EOF
        exit 1
    fi
    WORK="$(mktemp -d /tmp/fs-margins.XXXXXX)"
    # size= is the real guarantee here: the fill cannot exceed it no matter
    # what the steps ask for, so it can never grow into the rest of the system.
    mount -t tmpfs -o "size=${need}G,mode=0700" fs-margins "$WORK"
    TMPFS_MOUNTED=1
else
    have=$(( $(df -B1G --output=avail "$WORKDIR" | tail -1) ))
    echo "images:   $WORKDIR ($(df --output=fstype "$WORKDIR" | tail -1 | tr -d ' '))"
    echo "peak use: ~${peak}G disk   available: ${have}G"
    if [ "$have" -lt $(( need + 5 )) ]; then
        cat >&2 <<EOF

Refusing to run: needs ~$(( need + 5 ))G free in $WORKDIR, found ${have}G.

Filling the host filesystem is the failure this harness studies, not one it
should cause.

    WORKDIR=/mnt/scratch BACKING=disk sudo -E $0
    SIZES_GB="2" sudo -E $0
EOF
        exit 1
    fi
    WORK="$(mktemp -d "$WORKDIR/fs-margins.XXXXXX")"
fi
echo

# One random block reused for the whole fill. Incompressible, so neither the
# inner filesystem nor a compressing host can make the fill a lie, and far
# faster than pulling every byte from /dev/urandom.
BALLAST="$WORK/.ballast"
dd if=/dev/urandom of="$BALLAST" bs=1M count=32 status=none

fill_to() {
    local mp=$1 target_pct=$2
    local total want used need_bytes
    total=$(df -B1 --output=size "$mp" | tail -1)
    used=$(df -B1 --output=used "$mp" | tail -1)
    want=$(( total * target_pct / 100 ))
    need_bytes=$(( want - used ))
    [ "$need_bytes" -le 0 ] && return 0

    mkdir -p "$mp/ballast"
    local i=0
    while [ "$need_bytes" -gt 0 ]; do
        # Stop cleanly at ENOSPC rather than dying: running the inner
        # filesystem out of space is a legitimate step, not an error.
        cat "$BALLAST" >> "$mp/ballast/$i" 2>/dev/null || break
        need_bytes=$(( need_bytes - 32*1024*1024 ))
        # New file every 512M so ext4/xfs do not hit single-file limits
        [ $(( need_bytes / (32*1024*1024) % 16 )) -eq 0 ] && i=$(( i + 1 ))
    done
    sync
}

run_one() {
    local fstype=$1 size_gb=$2
    local img="$WORK/img.$fstype.$size_gb" mp="$WORK/mnt.$fstype.$size_gb"
    mkdir -p "$mp"

    truncate -s "${size_gb}G" "$img"
    case "$fstype" in
        btrfs) mkfs.btrfs -q -f "$img" >/dev/null 2>&1 || { rm -f "$img"; return 0; } ;;
        ext4)  mkfs.ext4 -q -F "$img" >/dev/null 2>&1 || { rm -f "$img"; return 0; } ;;
        xfs)   mkfs.xfs  -q -f "$img" >/dev/null 2>&1 || { rm -f "$img"; return 0; } ;;
    esac

    local lo
    lo=$(losetup --find --show "$img")
    if ! mount "$lo" "$mp" 2>/dev/null; then
        losetup -d "$lo"; rm -f "$img"; return 0
    fi

    local prev=-1
    for pct in $FILL_STEPS; do
        fill_to "$mp" "$pct"
        local actual
        actual=$(df --output=pcent "$mp" | tail -1 | tr -d ' %')
        # Once the filesystem is full, further steps ask for nothing and only
        # repeat the same line.
        if [ "$actual" = "$prev" ] && [ "$actual" -ge 99 ]; then continue; fi
        prev=$actual
        printf '%-6s %3sG  want%3s%% got%3s%%  ' "$fstype" "$size_gb" "$pct" "$actual"
        "$SPACECHECK" "$mp" "$CHUNK" 2>/dev/null || echo "spacecheck failed"
    done

    # Fill then delete, without balancing. This is the state the btrfs
    # unallocated check exists for and the one monotonic filling cannot reach:
    # chunks stay allocated after the data goes away, so free space returns
    # while unallocated does not. df looks healthy, metadata cannot grow.
    # On ext4 and xfs the space simply comes back and this should allow again,
    # which is the contrast worth seeing.
    local n=0
    for f in "$mp"/ballast/*; do
        [ -f "$f" ] || continue
        n=$(( n + 1 ))
        [ $(( n % 2 )) -eq 0 ] && rm -f "$f"
    done
    sync
    local after
    after=$(df --output=pcent "$mp" | tail -1 | tr -d ' %')
    printf '%-6s %3sG  deleted half  (%3s%%)  ' "$fstype" "$size_gb" "$after"
    "$SPACECHECK" "$mp" "$CHUNK" 2>/dev/null || echo "spacecheck failed"

    umount "$mp"
    losetup -d "$lo"
    rm -f "$img"
}

echo "Swap-file space policy vs real filesystems"
echo "ALLOW=false means the daemon would refuse to create a $CHUNK swap file."
echo

for fstype in $FSTYPES; do
    command -v "mkfs.$fstype" >/dev/null || { echo "skip $fstype (no mkfs)"; continue; }
    for size in $SIZES_GB; do
        run_one "$fstype" "$size"
    done
    echo
done
