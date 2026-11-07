#!/usr/bin/env bash
# Exercises watch mode: starts fansync on a tmpfs source (inside a user
# namespace, where fanotify filesystem marks are allowed on our own mounts),
# makes many kinds of changes, and checks the destination after each step.
# Run as: unshare -Urm tests/watch-test.sh
set -uo pipefail
D=$(cd "$(dirname "$0")" && pwd)
F=${F:-$D/../target/release/fansync}
T=${T:-/tmp/fansync-watch}
rm -rf "$T"; mkdir -p "$T"/{src,dst,outside}
if [[ ${SRCFS:-tmpfs} == ext4 ]]; then
    truncate -s 3G "$T.img" && mkfs.ext4 -q -F "$T.img" && mount -o loop "$T.img" "$T/src"
  else
    mount -t tmpfs -o size=2g src "$T/src"
  fi
python3 "$D/mktree.py" "$T/src"
mkdir -p "$T/src/excluded" && echo x > "$T/src/excluded/f"
LOG=$T/log
"$F" --delete -i --settle 0.3 --max-delay 3 --link-delay 600 --exclude=/excluded/ "${@}" "$T/src/" "$T/dst/" >"$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; wait $PID 2>/dev/null' EXIT
for _ in $(seq 100); do grep -q "watching for changes" "$LOG" && break; sleep 0.1; done
grep "watching for changes" "$LOG" || { cat "$LOG"; exit 1; }
fail=0
check() {
  sleep "${WAIT:-1.5}"
  if out=$(python3 "$D/treecmp.py" "$T/src" "$T/dst" --skip excluded); then
    echo "ok   $1"
  else
    echo "FAIL $1"; echo "$out" | sed 's/^/     /'; fail=1
  fi
}
cd "$T/src"
echo new > newfile; mkdir -p a/b/c && for i in $(seq 50); do echo $i > a/b/c/f$i; done
check "create files and nested dirs"
echo appended >> small.txt; printf 'Y' | dd of=big.bin bs=1 seek=40000000 conv=notrunc status=none
check "append and in-place overwrite"
before=$(grep -c '^>f' "$LOG")
mv big.bin big-renamed.bin; mv a a-renamed; mv hl/sub hl/sub-renamed
check "rename file, dir, dir with hard links"
after=$(grep -c '^>f' "$LOG")
[[ $after == "$before" ]] && echo "ok   renames copied no data" || { echo "FAIL renames copied $((after-before)) files"; fail=1; }
rm newfile; rm -r a-renamed/b
check "delete file and tree"
chmod 640 medium.bin; touch -d 2010-01-01 exec.sh; setfattr -n user.k -v v readonly 2>/dev/null || { chmod u+w readonly; setfattr -n user.k -v v readonly; chmod u-w readonly; }
setfacl -m u:$(id -u):r small.txt; mkdir -m 750 newdir; chmod 711 newdir
check "metadata changes"
ln medium.bin medium-link; ln -sfn exec.sh link-rel; ln -s x newlink
check "new hard link to a single-linked file, and symlinks"
echo changed-through-new-name >> medium-link
check "file modified through its new hard link"
mkdir -p linkdir && ln exec.sh linkdir/exec-link && ln readonly linkdir/ro-link
check "new hard links in another directory"
mkdir -p tree/a/b && for i in $(seq 30); do echo $i > tree/a/b/f$i; echo $i > tree/t$i; done
check "a tree of single-linked files"
cp -al tree tree-linked
check "cp -al of the tree"
grep -q "full pass (new hard links)" "$LOG" && { echo "FAIL needed the delayed re-link pass"; fail=1; } || echo "ok   all hard links made immediately"
mkdir -p "$T/outside/tree/x" && echo o > "$T/outside/tree/x/f" && mv "$T/outside/tree" moved-in
check "move a tree in from another filesystem"
mv emptydir "$T/outside/"
check "move a dir out"
for i in $(seq 5); do echo "v$i" > .save.tmp && mv .save.tmp saved.txt; done
check "atomic save pattern"
rm small.txt && mkdir small.txt && echo in > small.txt/in; rm -r private && echo file > private
check "type changes"
truncate -s 1G hole.img; printf 'Z' | dd of=hole.img bs=1 seek=500000000 conv=notrunc status=none
check "new sparse file"
echo changed > excluded/f; echo new > excluded/g
check "changes in an excluded dir are ignored"
[[ ! -e "$T/dst/excluded/g" ]] && echo "ok   excluded file not copied" || { echo "FAIL excluded file copied"; fail=1; }
( for i in $(seq 200); do echo "line $i" >> busy.log; sleep 0.01; done )
WAIT=4 check "file written continuously"
mkdir -p q && for i in $(seq 2000); do echo $i > q/$i; done; rm q/1*; mv q q2
WAIT=3 check "burst of 2000 creates, deletes and a rename"
echo; echo "--- fansync log (excerpt):"; grep -v '^[.>ch*c][fdLSh]' "$LOG" | tail -25
exit $fail
