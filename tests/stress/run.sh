#!/bin/bash
# usage: run.sh NAME COMMAND...
# Run COMMAND under monitor.sh and summarise what the kernel and the daemon did.
# Logs land in the current directory as load_NAME.log and mon_NAME.log.
set -u
name=$1
shift
here=$(dirname "$(readlink -f "$0")")
since=$(date "+%F %T")
werr0=$(sudo dmesg | grep -c 'Write-error')
oom0=$(sudo dmesg | grep -c 'Killed process')

# The log belongs to the caller; only the sampler needs root.
# shellcheck disable=SC2024
sudo "$here/monitor.sh" > "mon_$name.log" 2>&1 &
monitor=$!
"$@" > "load_$name.log" 2>&1
echo "exit=$?" >> "load_$name.log"
sleep 15
sudo kill "$monitor"

grep -E 'exit=|done|readback' "load_$name.log" | tr '\n' ' '
echo
printf 'write errors: %s  oom kills: %s\n' \
  $(($(sudo dmesg | grep -c 'Write-error') - werr0)) \
  $(($(sudo dmesg | grep -c 'Killed process') - oom0))
awk '{ram = 0; stored = 0
      for (i = 1; i <= NF; i++) {
        if ($i ~ /^phys=/) { split($i, a, "="); ram += a[2] }
        if ($i ~ /^orig=/) { split($i, a, "="); stored += a[2] }
      }
      if (ram > max_ram) max_ram = ram; if (stored > max_stored) max_stored = stored}
     END {printf "zram peak: %d MB stored in %d MB of RAM\n", max_stored, max_ram}' "mon_$name.log"
journalctl -u systemd-swap --since "$since" --no-pager -o cat \
  | grep -E 'expanding|removed|contracting|WARN|EMERG|pressure|wrote back' \
  | sed -E 's/[0-9]+/N/g' | sort | uniq -c
