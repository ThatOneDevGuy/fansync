#!/bin/bash
# Runs the test suites as real root, the way fansync runs on a host: real
# open_by_handle_at, an unlimited event queue, foreign owners, device nodes,
# trusted.* xattrs and file capabilities. Meant for a throwaway VM (it mounts
# filesystems and runs as root); see tests/vm.sh. ONLY="basic btrfs rsync
# fuzz" picks sections; FUZZ_ROUNDS sets the number of random rounds.
set -u
cd "$(dirname "$0")/.."
F=$PWD/target/release/fansync
fail=0
section() { echo; echo "##### $*"; }
result() { if [[ $1 == 0 ]]; then echo "PASS $2"; else echo "FAIL $2"; fail=1; fi; }
mkext4() { truncate -s "${2:-3G}" "$1.img"; mkfs.ext4 -q -F "$1.img"; mkdir -p "$1"; mount -o loop "$1.img" "$1"; }

want() { [[ -z ${ONLY:-} || " $ONLY " == *" $1 "* ]]; }

if want basic; then
section "root-only features on ext4"
R=/tmp/root-test; mkdir -p $R; mkext4 $R/src; mkext4 $R/dst
python3 tests/mktree.py $R/src/t
(
  cd $R/src/t
  chown 1234:5678 small.txt; chown -h 4321:8765 link-rel; chown 99:98 emptydir
  mknod chr c 1 3; mknod blk b 7 0; chown 7:6 chr; mknod fifo2 p
  chown 0:0 exec.sh; chmod 4755 exec.sh
  setfattr -n security.capability -v 0sAQAAAgAgAAAAAAAAAAAAAAAAAAA= exec.sh
  setfattr -n trusted.t -v secret medium.bin
  setfacl -m u:4242:rwx,g:4343:r emptydir; setfacl -d -m u:4242:rx emptydir
  mkdir -p owned/deep && chown -R 2000:2000 owned && chmod 2770 owned
)
$F --once -q --delete $R/src/t/ $R/dst/t/; result $? "fansync --once exit code"
python3 tests/treecmp.py $R/src/t $R/dst/t | tail -3; result ${PIPESTATUS[0]} "copy identical (owners, devices, caps, trusted xattrs, ACLs)"
[[ -z $(rsync -aHAXc -n -i --delete --numeric-ids $R/src/t/ $R/dst/t/) ]]; result $? "rsync -c finds no differences"
getfattr -n security.capability --only-values $R/dst/t/exec.sh 2>/dev/null | od -An -tx1 | grep -q "01 00 00 02"; result $? "file capability kept on a setuid file"
# Ownership and mode changes in an existing copy.
chown 5555:6666 $R/src/t/medium.bin; chmod 6755 $R/src/t/owned; chown -h 1:1 $R/src/t/link-abs
$F --once -q --delete $R/src/t/ $R/dst/t/; python3 tests/treecmp.py $R/src/t $R/dst/t | tail -1; result ${PIPESTATUS[0]} "owner/mode changes synced"
# A symlink swapped in where a file was must not redirect a chmod.
cp /bin/true $R/dst/victim; chmod 755 $R/dst/victim
echo x > $R/src/t/swap; $F --once -q $R/src/t/ $R/dst/t/
rm $R/dst/t/swap; ln -s $R/dst/victim $R/dst/t/swap; chmod 4777 $R/src/t/swap
$F --once -q $R/src/t/ $R/dst/t/ 2>/dev/null
[[ $(stat -c %a $R/dst/victim) == 755 ]]; result $? "chmod never follows a swapped-in symlink"
umount $R/src $R/dst

section "watch mode on tmpfs"
tests/watch-test.sh > /tmp/w.out 2>&1; grep -E "^(ok|FAIL)" /tmp/w.out | grep -v "^ok" ; result $(grep -c "^FAIL" /tmp/w.out) "watch-test (tmpfs): $(grep -c '^ok' /tmp/w.out) ok"
section "watch mode on ext4"
SRCFS=ext4 tests/watch-test.sh > /tmp/w.out 2>&1; grep -E "^FAIL" -A5 /tmp/w.out; result $(grep -c "^FAIL" /tmp/w.out) "watch-test (ext4): $(grep -c '^ok' /tmp/w.out) ok"
grep -q "isn't permitted\|indexing source" /tmp/fansync-watch/log; result $((1 - $?)) "uses open_by_handle_at (no fallback index)"

section "safety"
tests/safety-test.sh > /tmp/s.out 2>&1; grep "^FAIL" /tmp/s.out; result $(grep -c "^FAIL" /tmp/s.out) "safety-test: $(grep -c '^ok' /tmp/s.out) ok"

fi

if want btrfs; then
section "btrfs subvolumes"
tests/btrfs-test.sh > /tmp/b.out 2>&1; grep -v "^ok" /tmp/b.out; result $(grep -c "^FAIL" /tmp/b.out) "btrfs-test: $(grep -c '^ok' /tmp/b.out) ok"
fi

if want rsync; then
section "same results as rsync"
tests/vs-rsync.sh > /tmp/v.out 2>/dev/null; grep "^FAIL" -A5 /tmp/v.out; result $(grep -c "^FAIL" /tmp/v.out) "vs-rsync: $(grep -c '^ok' /tmp/v.out) ok"

fi

if want fuzz; then
section "random concurrent changes while watching (ext4)"
SRCFS=ext4 tests/fuzz-watch.sh "${FUZZ_ROUNDS:-20}" 8000 > /tmp/f.out 2>&1; grep -v "^ok" /tmp/f.out | head -20
result $(grep -c "^FAIL" /tmp/f.out) "fuzz: $(grep -c '^ok' /tmp/f.out) rounds exact"
fi

echo; [[ $fail == 0 ]] && echo "ALL PASSED" || echo "SOME FAILED"
