#!/bin/bash
# Sample memory, swap and zram state every 2 s until killed. Run as root:
# dmesg and zram's mm_stat need it.
while :; do
  printf '%s avail=%s swapfree=%s ' "$(date +%T)" \
    "$(awk '/MemAvailable/ {print int($2 / 1024)}' /proc/meminfo)" \
    "$(awk '/SwapFree/ {print int($2 / 1024)}' /proc/meminfo)"
  for d in /sys/block/zram[1-9]*; do
    [ -s "$d/mm_stat" ] && [ "$(cat "$d/disksize")" != 0 ] || continue
    # mm_stat: orig_data compr_data mem_used_total mem_limit ...
    printf '%s:%s ' "${d##*/}" "$(awk -v ds="$(cat "$d/disksize")" \
      '{printf "ds=%d orig=%d phys=%d ml=%d", ds / 1048576, $1 / 1048576, $3 / 1048576, $4 / 1048576}' "$d/mm_stat")"
  done
  printf 'files=%s werr=%s\n' "$(grep -c '^/swapfile' /proc/swaps)" \
    "$(dmesg | grep -c 'Write-error')"
  sleep 2
done
