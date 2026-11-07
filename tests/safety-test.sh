#!/usr/bin/env bash
# Safety checks. Run as: unshare -Urm tests/safety-test.sh
set -uo pipefail
D=$(cd "$(dirname "$0")" && pwd)
F=${F:-$D/../target/release/fansync}
T=/tmp/fansync-safety
fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }
fresh() { umount -R "$T"/* 2>/dev/null; rm -rf "$T"; mkdir -p "$T"/{src,dst}; }

# 1. A source that gets unmounted must not wipe the destination.
fresh; mount -t tmpfs s "$T/src"; for i in 1 2 3; do echo $i > "$T/src/f$i"; done
"$F" --delete --settle 0.2 --rescan 0.0005 "$T/src/" "$T/dst/" > "$T/log" 2>&1 & pid=$!
for _ in $(seq 50); do grep -q "watching" "$T/log" && break; sleep 0.1; done
umount -l "$T/src"; sleep 4; kill $pid; wait $pid 2>/dev/null
[[ -e $T/dst/f1 && -e $T/dst/f3 ]] && grep -q "different filesystem" "$T/log" && ok "unmounted source leaves the backup alone" ||
  { bad "unmounted source"; tail -5 "$T/log"; }

# 2. An empty source next to a full destination is refused with --delete.
fresh; echo keep > "$T/dst/keep"
"$F" --once --delete "$T/src/" "$T/dst/" > "$T/log" 2>&1; rc=$?
[[ -e $T/dst/keep && $rc != 0 ]] && ok "empty source refused (exit $rc)" || bad "empty source deleted the destination"

# 3. Deletion never goes into a filesystem mounted inside the destination.
fresh; echo x > "$T/src/x"; mkdir -p "$T/dst/gone/m"; mount -t tmpfs m "$T/dst/gone/m"; echo data > "$T/dst/gone/m/data"
"$F" --once --delete "$T/src/" "$T/dst/" > "$T/log" 2>&1
[[ -e $T/dst/gone/m/data ]] && ok "mount inside the destination kept" || bad "deleted inside a mounted filesystem"
umount "$T/dst/gone/m"

# 4. Watch mode picks up metadata changes on the source directory itself.
fresh; mount -t tmpfs s "$T/src"; chmod 755 "$T/src"; echo a > "$T/src/a"
"$F" --delete --settle 0.2 "$T/src/" "$T/dst/" > "$T/log" 2>&1 & pid=$!
for _ in $(seq 50); do grep -q "watching" "$T/log" && break; sleep 0.1; done
chmod 701 "$T/src"; setfattr -n user.root -v 1 "$T/src" 2>/dev/null; sleep 1.5; kill $pid; wait $pid 2>/dev/null
[[ $(stat -c %a "$T/dst") == 701 ]] && ok "source root's own metadata synced" || { bad "root metadata: $(stat -c %a "$T/dst")"; tail -3 "$T/log"; }
umount "$T/src"

# 5. Renames aren't mirrored without --delete (rsync keeps the old name).
fresh; mount -t tmpfs s "$T/src"; echo a > "$T/src/a"
"$F" --settle 0.2 "$T/src/" "$T/dst/" > "$T/log" 2>&1 & pid=$!
for _ in $(seq 50); do grep -q "watching" "$T/log" && break; sleep 0.1; done
mv "$T/src/a" "$T/src/b"; sleep 1.5; kill $pid; wait $pid 2>/dev/null
[[ -e $T/dst/a && -e $T/dst/b ]] && ok "no --delete: old name kept after rename" || bad "rename without --delete"
umount "$T/src"

# 6. Source inside the destination is refused.
fresh; mkdir -p "$T/dst/sub"; echo s > "$T/dst/sub/s"
"$F" --once --delete "$T/dst/sub/" "$T/dst/" > "$T/log" 2>&1; rc=$?
[[ $rc != 0 && -e $T/dst/sub/s ]] && ok "source inside destination refused" || bad "source inside destination"
# 7. A bind mount of the same filesystem inside the source is a mount point
#    too (same device number, different mount): not copied a second time,
#    in full passes or while watching.
fresh; mount -t tmpfs s "$T/src"; mkdir -p "$T/src/real" "$T/src/bind"; echo data > "$T/src/real/f"
mount --bind "$T/src/real" "$T/src/bind"
"$F" --delete --settle 0.2 "$T/src/" "$T/dst/" > "$T/log" 2>&1 & pid=$!
for _ in $(seq 50); do grep -q "watching" "$T/log" && break; sleep 0.1; done
echo more > "$T/src/real/g"; sleep 1.5; kill $pid; wait $pid 2>/dev/null
[[ -d $T/dst/bind && -z $(ls -A "$T/dst/bind") && -e $T/dst/real/g ]] && ok "bind mount in the source copied as an empty directory" ||
  bad "bind mount in the source: dst/bind has: $(ls -A "$T/dst/bind" 2>&1)"
umount "$T/src/bind" "$T/src"

# 8. --delete never deletes through a bind mount inside the destination,
#    even of the same filesystem (rsync -x does).
fresh; mount -t tmpfs d "$T/dst"; mkdir -p "$T/dst/elsewhere" "$T/dst/backup/gone"; echo keep > "$T/dst/elsewhere/important"
mount --bind "$T/dst/elsewhere" "$T/dst/backup/gone"; echo x > "$T/src/x"
"$F" --once --delete "$T/src/" "$T/dst/backup/" > "$T/log" 2>&1
umount "$T/dst/backup/gone"
[[ -e $T/dst/elsewhere/important ]] && ok "bind mount in the destination kept by --delete" || bad "deleted through a bind mount in the destination"
umount "$T/dst"

# 9-11. A failing destination drive (it goes read-only, or disappears):
#    --exit-on-error ends fansync with code 11 and says why, in the log and
#    the status file; without it, fansync keeps running and retries.
watch_dst() { # ARGS...: watches src/ -> dst/, waits until it's watching
  "$F" --settle 0.2 --status-file "$T/status" "$@" "$T/src/" "$T/dst/" > "$T/log" 2>&1 & pid=$!
  for _ in $(seq 50); do grep -q "watching" "$T/log" && break; sleep 0.1; done
}
ended() { # sets rc to fansync's exit code, or "running" after 5 s
  for _ in $(seq 50); do kill -0 $pid 2>/dev/null || break; sleep 0.1; done
  if kill -0 $pid 2>/dev/null; then rc=running; kill $pid; wait $pid 2>/dev/null; else wait $pid; rc=$?; fi
}
fresh; mount -t tmpfs s "$T/src"; mount -t tmpfs d "$T/dst"; echo a > "$T/src/a"
watch_dst --exit-on-error
mount -o remount,bind,ro "$T/dst"; echo b > "$T/src/b"; ended
[[ $rc == 11 ]] && grep -q "stopping, as --exit-on-error asks: .*Read-only" "$T/log" &&
  grep -q "^state='failed'" "$T/status" && grep -q "^error='.*Read-only" "$T/status" &&
  ok "--exit-on-error: read-only destination ends fansync (exit $rc)" || { bad "--exit-on-error, read-only destination (rc $rc)"; tail -3 "$T/log"; }
fresh; mount -t tmpfs s "$T/src"; mount -t tmpfs d "$T/dst"; echo a > "$T/src/a"
watch_dst --exit-on-error
umount -l "$T/dst"; echo b > "$T/src/b"; ended
[[ $rc == 11 ]] && grep -q "stopping, as --exit-on-error asks: .*different filesystem" "$T/log" &&
  ok "--exit-on-error: unmounted destination ends fansync (exit $rc)" || { bad "--exit-on-error, unmounted destination (rc $rc)"; tail -3 "$T/log"; }
fresh; mount -t tmpfs s "$T/src"; mount -t tmpfs d "$T/dst"; echo a > "$T/src/a"
watch_dst
mount -o remount,bind,ro "$T/dst"; echo b > "$T/src/b"; ended
[[ $rc == running ]] && grep -q "will retry" "$T/log" && [[ $(grep -c "^.*error:" "$T/log") == 1 ]] &&
  ok "without --exit-on-error: one error, keeps running and retries" || { bad "without --exit-on-error (rc $rc)"; tail -3 "$T/log"; }
umount "$T/dst"
exit $fail
