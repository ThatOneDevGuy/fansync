#!/usr/bin/env bash
# Failing drives, simulated with device-mapper: a filesystem's device is
# switched (wholly or in part) to the "error" target, so its I/O fails with
# EIO, like a drive that dropped out or has bad sectors. Needs real root
# (run it in the VM, see vm-tests.sh).
set -uo pipefail
D=$(cd "$(dirname "$0")" && pwd)
F=${F:-$D/../target/release/fansync}
T=/tmp/fansync-drive
export DM_DISABLE_UDEV=1
fail=0
pid=
n=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; tail -5 "$T/log" | sed 's/^/     /'; fi; }

# An ext4 filesystem on a fresh dm device, mounted at $2; sets DM and SZ.
dmfs() {
  n=$((n + 1)); DM=fansync-$1$n
  truncate -s 256M "/tmp/drive$n.img"
  local l; l=$(losetup -f --show "/tmp/drive$n.img"); SZ=$(blockdev --getsz "$l")
  dmsetup create "$DM" --table "0 $SZ linear $l 0"
  mkfs.ext4 -q -F "/dev/mapper/$DM"; mkdir -p "$2"; mount -o commit=1 "/dev/mapper/$DM" "$2"
  LOOP=$l
}
# Swaps device $1's table: every read and write fails from now on (or only
# sectors $2..$3, if given). Caches are dropped so reads reach the device.
broken() {
  local t="0 $SZ error"
  [[ $# == 3 ]] && t="0 $2 linear $LOOP 0
$2 $(($3 - $2)) error
$3 $((SZ - $3)) linear $LOOP $3"
  dmsetup suspend "$1" && dmsetup load "$1" --table "$t" && dmsetup resume "$1"
  sync; echo 3 > /proc/sys/vm/drop_caches
}
fresh() {
  [[ -n $pid ]] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null; pid=
  [[ -n ${pin:-} ]] && kill "$pin" 2>/dev/null; pin=
  umount -l "$T"/* 2>/dev/null; rm -rf "$T"; mkdir -p "$T/src"
  mount -t tmpfs s "$T/src"; mkdir "$T/src/many"
  for i in $(seq 200); do echo "$i" > "$T/src/many/f$i"; done
}
start() { # ARGS...: watches src/ -> dst/backup/ until it's watching
  "$F" --delete --settle 0.3 --status-file "$T/status" "$@" "$T/src/" "$T/dst/backup/" > "$T/log" 2>&1 & pid=$!
  for _ in $(seq 100); do grep -q "watching for changes" "$T/log" && break; sleep 0.1; done
  # Keeps the destination's directory cached, so the failure shows up in
  # the middle of a pass rather than when it starts.
  (cd "$T/dst/backup" && exec sleep 1000) & pin=$!
}
ended() { # sets rc to fansync's exit code, or "running" after 20 s
  for _ in $(seq 200); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
  if kill -0 "$pid" 2>/dev/null; then rc=running; else wait "$pid"; rc=$?; pid=; fi
}

# The destination drive dies while fansync watches: with --exit-on-error it
# stops, says why, and marks the status file.
fresh; dmfs dst "$T/dst"; start --exit-on-error
broken "$DM"; touch "$T/src/many/"*; ended
check "--exit-on-error: a dead destination drive ends fansync (exit $rc)" \
  '[[ $rc == 11 ]] && grep -q "stopping, as --exit-on-error asks: .*Input/output error" "$T/log"'
check "  ...and the status file says so" 'grep -q "^state=.failed" "$T/status" && grep -q "^error=.*Input/output error" "$T/status"'

# Without --exit-on-error: one error for the pass, not one per file, and
# fansync keeps running, retrying later.
fresh; dmfs dst "$T/dst"; start
broken "$DM"; touch "$T/src/many/"*; ended
check "a dead destination drive stops the pass after one error, fansync keeps running" \
  '[[ $rc == running && $(grep -c "error:" "$T/log") == 1 ]] && grep -q "Input/output error.*; stopping this pass" "$T/log" && grep -q "synced.* 1 error$" "$T/log" && grep -q "will retry" "$T/log"'
fresh

# A bad sector in a source file fails that file only: the rest of the pass
# goes on, even with --exit-on-error. (In-kernel copies can't tell which
# side failed; fansync retries the chunk through user space to find out.)
dmfs src "$T/bad"; head -c 4M /dev/urandom > "$T/bad/bad.bin"; mkdir "$T/bad/ok"
for i in $(seq 50); do echo "$i" > "$T/bad/ok/f$i"; done; sync
first=$(filefrag -v "$T/bad/bad.bin" | awk '$1 == "0:" { sub(/\.\..*/, "", $4); print $4 }')
broken "$DM" $((first * 8 + 64)) $((first * 8 + 128))
mkdir -p "$T/dst2"; mount -t tmpfs d "$T/dst2"
"$F" --once --exit-on-error "$T/bad/" "$T/dst2/" > "$T/log" 2>&1; rc=$?
check "a bad sector in a source file fails that file only (exit $rc)" \
  '[[ $rc == 23 && $(ls "$T/dst2/ok" | wc -l) == 50 ]] && grep -q "read bad.bin: Input/output error" "$T/log" && ! grep -q "stopping" "$T/log"'

[[ -n $pid ]] && kill "$pid" 2>/dev/null
[[ -n ${pin:-} ]] && kill "$pin" 2>/dev/null
umount -l "$T"/* 2>/dev/null
[[ $fail == 0 ]] && echo "all drive failure tests passed"
exit $fail
