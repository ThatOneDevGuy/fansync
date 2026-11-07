#!/usr/bin/env bash
# Watch a running sync-to-renegade.sh (fansync version) and raise desktop
# alerts (notify-send) when something needs attention. Prints one line per
# event. No sudo needed. Re-running it picks up where the last run left off.
state=${XDG_RUNTIME_DIR:-/tmp}/drive-sync-watch.state
STATUS=${STATUS:-/run/drive-sync.status}
first_pass_reported=0 errors_reported=0
[[ -f $state ]] && source "$state"

alert() {  # alert URGENCY MESSAGE
  echo "$2"
  notify-send -a drive-sync -u "$1" "Drive sync" "$2" 2>/dev/null || true
}
save() { printf 'first_pass_reported=%s\nerrors_reported=%s\n' "$first_pass_reported" "$errors_reported" > "$state"; }
temp_of() { echo $(( $(cat /sys/class/nvme/"$1"/hwmon*/temp1_input) / 1000 )); }

# fansync keeps $STATUS current: state, pair, pid, errors, written_bytes, ...
read_status() {
  local key val
  while IFS='=' read -r key val; do
    val=${val#\'}; val=${val%\'}
    printf -v "st_$key" '%s' "$val"
  done < "$STATUS"
}
[[ -r $STATUS ]] || { echo "no status file at $STATUS; is sync-to-renegade.sh running?"; exit 0; }
read_status
[[ -d /proc/$st_pid ]] || { echo "fansync isn't running"; exit 0; }
pid=$st_pid

last_log=$(date +%s) hot= low= last_written=$st_written_bytes last_growth=$(date +%s)
while :; do
  if [[ ! -d /proc/$pid ]]; then
    read_status
    case $st_state in
      stopped) alert normal "The sync was stopped." ;;
      initial-sync|syncing) alert critical "The sync stopped while copying $st_pair. If you didn't press Ctrl-C, check its terminal for the error." ;;
      *) alert critical "The sync stopped unexpectedly. Check its terminal for the error." ;;
    esac
    exit 0
  fi
  read_status

  if [[ $st_state == watching && $first_pass_reported == 0 ]]; then
    first_pass_reported=1; save
    alert normal "First sync pass finished ($(df -h --output=used /mnt/renegade | tail -1 | xargs) on the Renegade). From now on changes are copied as they happen. You can run setup-boot.sh now."
  fi
  if (( st_errors > errors_reported )); then
    alert critical "fansync reported $((st_errors - errors_reported)) new error(s); check its terminal."
    errors_reported=$st_errors; save
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
  if [[ $st_written_bytes != "$last_written" ]]; then last_written=$st_written_bytes last_growth=$now; fi
  if [[ $st_state == initial-sync ]] && (( now - last_growth > 900 )); then
    alert normal "The first sync pass hasn't written anything for 15 minutes (on $st_pair). It may be checking a large folder that's already copied, or it may be stuck."
    last_growth=$now
  fi

  sleep 5
done
