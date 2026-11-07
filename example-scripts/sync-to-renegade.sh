#!/usr/bin/env bash
# Copy the running system on the 990 PRO into the Renegade's subvolumes.
# Repeats until you press Ctrl-C so the copy stays close to current. Each pass
# only transfers what changed, and an interrupted pass picks up next time.
#   --test        check every copy step's direction and folder mapping using
#                 marker files only, then clean up. Nothing else is touched.
#   --once        run one pass and exit
#   --dry-run     list what would change without copying (implies --once)
#   --interval N  minutes between passes (default 10)
set -euo pipefail
source "$(dirname "$(readlink -f "$0")")/config.sh"
need_root
need_mounted

mode=loop interval=10
while (( $# )); do
  case $1 in
    --test) mode=test ;;
    --once) mode=once ;;
    --dry-run) mode=dry ;;
    --interval) interval=${2:?--interval needs a number of minutes}; shift ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done
RSYNC_OPTS=(-aHAXSx --numeric-ids --delete)
case $mode in
  dry) RSYNC_OPTS+=(--dry-run --itemize-changes) ;;
  loop|once) RSYNC_OPTS+=(--info=progress2) ;;
esac

# Only ever copy from the 990 PRO to the Renegade, never the reverse.
root_disk=$(lsblk -no PKNAME "$(findmnt -nvo SOURCE /)" 2>/dev/null || true)
[[ /dev/$root_disk == "$(readlink -f "$OLD_DISK")" ]] ||
  die "this copies the 990 PRO's system, so it must run from that system"

R=$REN_TOP/$OS_GROUP/root

# Call FUNC once per copy step: FUNC LABEL SRC DEST [extra rsync options]
for_each_step() {
  local f=$1
  # The OS root partition. Directories that come from the persistent
  # partition are filled by later steps, so their contents are excluded here,
  # which also keeps --delete away from them.
  local skip=(/home/'*' /opt/'*' /usr/share/'*' /usr/local/'*' /var/'*'
              /scripts/'*' /tools/'*' /data-current/'*' /data-v1/'*' /data-bulk/'*'
              /archives/'*' /tmp/'*' /lost+found /data /swap "${RENEGADE_ONLY[@]}")
  $f "system (/)" / "$R/" "${skip[@]/#/--exclude=}"
  $f "/opt" "$OLD_OS/opt/" "$R/opt/"
  $f "/usr/share" "$OLD_OS/usr/share/" "$R/usr/share/"
  $f "/usr/local" "$OLD_OS/usr/local/" "$R/usr/local/"
  $f "/var" "$OLD_OS/var/" "$R/var/" --exclude='/log/*'
  $f "/var/log" "$OLD_OS/var/log/" "$REN_TOP/$OS_GROUP/var-log/"
  # Workspaces are bind-mounted inside home and come from the shared data below.
  $f "/home" "$OLD_OS/home/" "$REN_TOP/$OS_GROUP/home/" \
    --exclude="/$USER_NAME/workspaces/*" --exclude="/$USER_NAME/.local/share/containers/*"
  $f "podman storage" "$OLD_OS/home/$USER_NAME/.local/share/containers/" "$REN_TOP/shared/containers/"
  $f "shared data" "$OLD_DATA/" "$REN_TOP/shared/data/" \
    --exclude="/$(basename "$OLD_OS")" --exclude=/lost+found
}

copy() {
  local label=$1 src=$2 dst=$3 rc=0
  shift 3
  if [[ ! -d $src ]]; then echo "-- $label: $src not found, skipped"; return; fi
  echo "-- $label"
  rsync "${RSYNC_OPTS[@]}" "$@" "$src" "$dst" || rc=$?
  # 24 means files vanished mid-copy, which is normal on a running system.
  [[ $rc == 0 || $rc == 24 ]] || die "rsync failed copying $label (exit $rc)"
}

# --test: marker files only. The filter puts the markers first and excludes
# everything else, and excluded files are never deleted, so the real data on
# both sides is untouched.
token=drive-sync-test-$$-$RANDOM
markers=()
failures=0
cleanup_markers() { (( ${#markers[@]} )) && rm -f "${markers[@]}"; return 0; }

check() {  # check DESCRIPTION COMMAND...
  local desc=$1
  shift
  if "$@"; then echo "   ok    $desc"; else echo "   FAIL  $desc"; failures=$((failures + 1)); fi
}

# The physical drive a path is stored on, looked up independently of the step
# definitions, e.g. /dev/nvme1n1 for the 990 PRO.
disk_of() {
  local dev
  dev=$(findmnt -nvo SOURCE -T "$1") || return 0
  lsblk -srno NAME,TYPE "$dev" | awk '$2 == "disk" {print "/dev/" $1; exit}'
}

test_copy() {
  local label=$1 src=$2 dst=$3
  shift 3
  if [[ ! -d $src ]]; then echo "-- $label: $src not found, skipped"; return; fi
  echo "-- $label: $src -> $dst"
  local on_src=$src.$token-on-990 on_dst=$dst.$token-on-renegade before=$failures
  mkdir -p "$dst"
  check "source is on the 990 PRO" [ "$(disk_of "$src")" = "$(readlink -f "$OLD_DISK")" ]
  check "destination is on the Renegade" [ "$(disk_of "$dst")" = "$(readlink -f "$REN_DISK")" ]
  if (( failures > before )); then echo "   skipping the copy test for this step"; return; fi
  markers+=("$on_src" "$on_dst" "$src.$token-on-renegade" "$dst.$token-on-990")
  echo "$token" > "$on_src"
  echo "$token" > "$on_dst"
  rsync "${RSYNC_OPTS[@]}" --include="/.$token-*" --exclude='*' "$@" "$src" "$dst"
  check "990 PRO file copied to the Renegade" [ "$(cat "$dst.$token-on-990" 2>/dev/null)" = "$token" ]
  check "Renegade-only file deleted" [ ! -e "$on_dst" ]
  check "nothing copied back to the 990 PRO" [ ! -e "$src.$token-on-renegade" ]
  rm "$on_src"
  rsync "${RSYNC_OPTS[@]}" --include="/.$token-*" --exclude='*' "$@" "$src" "$dst"
  check "deletion carried over to the Renegade" [ ! -e "$dst.$token-on-990" ]
}

ensure_subvolumes
take_lock

if [[ $mode == test ]]; then
  trap cleanup_markers EXIT
  for_each_step test_copy
  if (( failures )); then die "$failures check(s) failed. Don't run the full sync."; fi
  echo "All checks passed: every step copies from the 990 PRO to the Renegade."
  exit 0
fi

pass=0
while :; do
  pass=$((pass + 1))
  echo "== Pass $pass started $(date +%H:%M)"
  sync_pass_start=$SECONDS
  for_each_step copy
  echo "== Pass $pass finished $(date +%H:%M) ($(( (SECONDS - sync_pass_start) / 60 )) min)"
  [[ $mode == loop ]] || break
  release_lock
  echo "   Next pass in $interval minutes. Press Ctrl-C to stop."
  sleep $((interval * 60))
  take_lock
done

