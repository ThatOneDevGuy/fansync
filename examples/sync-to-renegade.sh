#!/usr/bin/env bash
# Keep the Renegade's subvolumes an always-current copy of the running system
# on the 990 PRO. Same steps, excludes and safety checks as the rsync version,
# but fansync does one full pass and then copies each change within seconds of
# it being made (fanotify), plus a full safety pass every few hours. Stop with
# Ctrl-C; an interrupted pass picks up next time.
#   --test        check every copy step's direction and folder mapping using
#                 marker files only, then clean up. Nothing else is touched.
#   --once        run one full pass and exit
#   --dry-run     list what would change without copying (implies --once)
#   --interval N  hours between full safety passes while watching (default 6)
set -euo pipefail
source "$(dirname "$(readlink -f "$0")")/config.sh"
need_root
need_mounted
FANSYNC=${FANSYNC:-$(command -v fansync || echo /usr/local/bin/fansync)}
# Read by watch-sync.sh.
STATUS=${STATUS:-/run/drive-sync.status}
[[ -x $FANSYNC ]] || die "fansync not found (set FANSYNC=/path/to/fansync)"

mode=watch rescan=6
while (( $# )); do
  case $1 in
    --test) mode=test ;;
    --once) mode=once ;;
    --dry-run) mode=dry ;;
    --interval) rescan=${2:?--interval needs a number of hours}; shift ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done

# Only ever copy from the 990 PRO to the Renegade, never the reverse.
root_disk=$(lsblk -no PKNAME "$(findmnt -nvo SOURCE /)" 2>/dev/null || true)
[[ /dev/$root_disk == "$(readlink -f "$OLD_DISK")" ]] ||
  die "this copies the 990 PRO's system, so it must run from that system"

R=$REN_TOP/$OS_GROUP/root

# Call FUNC once per copy step: FUNC LABEL SRC DEST [--exclude=... ...]
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

# Writes one step as a [[pair]] of the fansync config.
toml_str() { local s=${1//\\/\\\\}; s=${s//\"/\\\"}; printf '"%s"' "$s"; }
toml_step() {
  local label=$1 src=$2 dst=$3 x
  shift 3
  if [[ ! -d $src ]]; then echo "-- $label: $src not found, skipped" >&2; return; fi
  printf '\n[[pair]]\nname = %s\nsrc = %s\ndst = %s\nrules = [' "$(toml_str "$label")" "$(toml_str "$src")" "$(toml_str "$dst")"
  for x in "$@"; do
    case $x in
      --exclude=*) printf '\n  %s,' "$(toml_str "- ${x#--exclude=}")" ;;
      --include=*) printf '\n  %s,' "$(toml_str "+ ${x#--include=}")" ;;
      *) die "step $label: unsupported option $x" ;;
    esac
  done
  printf '\n]\n'
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
  "$FANSYNC" --once -q --delete --include="/.$token-*" --exclude='*' "$@" "$src" "$dst"
  check "990 PRO file copied to the Renegade" [ "$(cat "$dst.$token-on-990" 2>/dev/null)" = "$token" ]
  check "Renegade-only file deleted" [ ! -e "$on_dst" ]
  check "nothing copied back to the 990 PRO" [ ! -e "$src.$token-on-renegade" ]
  rm "$on_src"
  "$FANSYNC" --once -q --delete --include="/.$token-*" --exclude='*' "$@" "$src" "$dst"
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

cfg=$(mktemp /run/drive-sync.XXXXXX.toml)
trap 'rm -f "$cfg"' EXIT
{
  echo "delete = true"
  echo "rescan = $rescan"
  echo "status_file = $(toml_str "$STATUS")"
  for_each_step toml_step
} > "$cfg"

case $mode in
  dry) "$FANSYNC" --config "$cfg" --dry-run ;;
  once) "$FANSYNC" --config "$cfg" --once --progress ;;
  # The lock is held for as long as fansync runs, since it keeps writing.
  watch) "$FANSYNC" --config "$cfg" --progress ;;
esac
