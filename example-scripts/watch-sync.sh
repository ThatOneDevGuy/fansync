#!/usr/bin/env bash
# Watch a running sync-to-renegade.sh and raise desktop alerts (notify-send)
# when something needs attention. Prints one line per event. No sudo needed.
# Re-running it picks up where the last run left off.
state=${XDG_RUNTIME_DIR:-/tmp}/drive-sync-watch.state
passes_reported=0
[[ -f $state ]] && source "$state"

alert() {  # alert URGENCY MESSAGE
  echo "$2"
  notify-send -a drive-sync -u "$1" "Drive sync" "$2" 2>/dev/null || true
}
used_bytes() { df -B1 --output=used /mnt/renegade 2>/dev/null | tail -1 | tr -d ' '; }
temp_of() { echo $(( $(cat /sys/class/nvme/"$1"/hwmon*/temp1_input) / 1000 )); }

pid=$(pgrep -f '^bash /home/sen/drive-sync/sync-to-renegade.sh' | head -1)
[[ $pid ]] || { echo "sync-to-renegade.sh isn't running"; exit 0; }

phase=start step= last_log=$(date +%s) hot= low=
last_used=$(used_bytes) last_growth=$(date +%s)
while :; do
  if [[ ! -d /proc/$pid ]]; then
    if [[ $phase == copying ]]; then
      alert critical "The sync stopped while copying $step. If you didn't press Ctrl-C, check its terminal for the error."
    else
      alert normal "The sync stopped between passes, so it looks like it was stopped on purpose."
    fi
    exit 0
  fi

  rsync_pid=$(pgrep -P "$pid" -x rsync | head -1)
  if [[ $rsync_pid ]]; then
    phase=copying
    step=$(ps -o args= -p "$rsync_pid" | awk '{print $NF}')
    step=${step#/mnt/renegade}
  elif pgrep -P "$pid" -x sleep >/dev/null; then
    if [[ $phase == copying || ($phase == start && $passes_reported == 0) ]]; then
      passes_reported=$((passes_reported + 1))
      echo "passes_reported=$passes_reported" > "$state"
      if (( passes_reported == 1 )); then
        alert normal "First sync pass finished ($(df -h --output=used /mnt/renegade | tail -1 | xargs) on the Renegade). You can run setup-boot.sh now."
      fi
    fi
    phase=between
  fi

  # Kernel trouble with disks, encryption or filesystems. Podman's overlayfs
  # notices are routine and ignored.
  now=$(date +%s)
  errors=$(journalctl -k -p warning --since "@$last_log" -q -o cat --no-pager 2>/dev/null |
    grep -v overlayfs | grep -iE 'btrfs|nvme|i/o error|blk_|dm-[0-9]|crypt|ext4|critical|medium error' | head -3)
  last_log=$now
  [[ $errors ]] && alert critical "Kernel reported a disk or filesystem problem: ${errors%%$'\n'*}"

  for dev in nvme0 nvme1; do
    t=$(temp_of $dev)
    if (( t >= 75 )) && [[ $hot != *$dev* ]]; then
      hot+=" $dev"
      alert critical "$(xargs < /sys/class/nvme/$dev/model) is at ${t}°C (throttles around 82°C)."
    fi
  done

  avail=$(df -B1 --output=avail /mnt/renegade 2>/dev/null | tail -1 | tr -d ' ')
  if [[ $avail && $avail -lt 100000000000 && ! $low ]]; then
    low=1
    alert critical "The Renegade has less than 100 GB free."
  fi

  # During the first pass something should be written every few minutes.
  used=$(used_bytes)
  if [[ $used != "$last_used" ]]; then last_used=$used last_growth=$now; fi
  if [[ $phase == copying ]] && (( passes_reported == 0 && now - last_growth > 900 )); then
    alert normal "The first sync pass hasn't written anything for 15 minutes (on $step). It may be scanning a large folder, or it may be stuck."
    last_growth=$now
  fi

  sleep 5
done

