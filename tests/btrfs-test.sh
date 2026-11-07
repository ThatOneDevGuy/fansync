#!/usr/bin/env bash
# btrfs subvolumes as sources, laid out like a backup of a running system:
# the system writes through its own subvol= mounts, and fansync reads the
# filesystem's top level, mounted read-only elsewhere, with one pair per
# subvolume. Needs real root and btrfs (run it in the VM, see vm-tests.sh).
set -uo pipefail
D=$(cd "$(dirname "$0")" && pwd)
F=${F:-$D/../target/release/fansync}
T=/tmp/fansync-btrfs
fail=0
pid=
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }
stop() { [[ -n $pid ]] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null; pid=; }
start() { # LOG ARGS...: runs fansync in watch mode and waits until it watches
  local log=$1; shift
  "$F" -v --settle 0.3 --max-delay 3 "$@" > "$log" 2>&1 & pid=$!
  for _ in $(seq 100); do grep -q "watching for changes" "$log" && return; sleep 0.1; done
  cat "$log"
}

modprobe btrfs 2>/dev/null
umount -R "$T"/* 2>/dev/null; rm -rf "$T"; mkdir -p "$T"/{top,ro,sysroot,syshome,dst}
truncate -s 1G "$T.img"; mkfs.btrfs -q -f "$T.img" > /dev/null
L=$(losetup -f --show "$T.img")
trap 'stop; umount -R "$T"/* 2>/dev/null; losetup -d "$L"' EXIT
mount "$L" "$T/top"
mkdir -p "$T/top/os" "$T/top/shared"
for v in os/root os/home shared/data os/root/nested; do btrfs -q subvolume create "$T/top/$v"; done
echo r > "$T/top/os/root/rfile"; echo h > "$T/top/os/home/hfile"; echo d > "$T/top/shared/data/dfile"
echo n > "$T/top/os/root/nested/inner"; head -c 100000 /dev/urandom > "$T/top/os/home/big"
mount -o ro,subvolid=5 "$L" "$T/ro"
mount -o subvol=os/root "$L" "$T/sysroot"; mount -o subvol=os/home "$L" "$T/syshome"

# What the kernel itself does (Linux 6.8+): no filesystem mark on a
# subvolume, but one on a directory of the top level is fine.
mark() {
  python3 - "$1" <<'PY'
import ctypes, os, sys
c = ctypes.CDLL(None, use_errno=True)
c.fanotify_mark.argtypes = [ctypes.c_int, ctypes.c_uint, ctypes.c_uint64, ctypes.c_int, ctypes.c_char_p]
fd = c.fanotify_init(0xc00, 0)  # FAN_REPORT_DFID_NAME
r = c.fanotify_mark(fd, 0x101, 0x100, -100, sys.argv[1].encode())  # ADD|FILESYSTEM, FAN_CREATE
print("ok" if r == 0 else os.strerror(ctypes.get_errno()))
PY
}
r1=$(mark "$T/ro/os/root"); r2=$(mark "$T/ro/os")
check "kernel: subvolume mark refused ($r1), top-level directory accepted ($r2)" \
  '[[ $r1 == "Invalid cross-device link" && $r2 == ok ]]'

# One pair per subvolume, all through the read-only top-level mount.
cat > "$T/cfg.toml" <<EOF
delete = true
[[pair]]
src = "$T/ro/os/root/"
dst = "$T/dst/root/"
[[pair]]
src = "$T/ro/os/home/"
dst = "$T/dst/home/"
[[pair]]
src = "$T/ro/shared/data/"
dst = "$T/dst/data/"
EOF
start "$T/log" --config "$T/cfg.toml"
check "all 3 subvolume pairs watched" 'grep -q "3 of 3 pairs through fanotify" "$T/log"'
check "watched through the top level" 'grep -q "a btrfs subvolume, watched through $T/ro/os\$" "$T/log"'
same() { python3 "$D/treecmp.py" "$1" "$2" --skip nested > /dev/null; }
synced() { same "$T/ro/os/root" "$T/dst/root" && same "$T/ro/os/home" "$T/dst/home" && same "$T/ro/shared/data" "$T/dst/data"; }
check "initial pass" synced
check "nested subvolume copied as an empty directory" '[[ -d $T/dst/root/nested && -z $(ls -A "$T/dst/root/nested") ]]'

echo new > "$T/sysroot/newfile"; mkdir -p "$T/sysroot/a/b"; echo deep > "$T/sysroot/a/b/f"
echo h2 >> "$T/syshome/hfile"; echo d2 > "$T/top/shared/data/d2"
sleep 2
check "changes in every subvolume synced as they happen" '[[ -e $T/dst/root/a/b/f && -e $T/dst/data/d2 ]] && synced'
mv "$T/sysroot/a" "$T/sysroot/a2"; rm "$T/syshome/hfile"; ln "$T/syshome/big" "$T/syshome/big2"
sleep 2
check "rename, delete and hard link" \
  'synced && [[ ! -e $T/dst/home/hfile && $(stat -c %i "$T/dst/home/big") == $(stat -c %i "$T/dst/home/big2") ]]'
check "rename mirrored, not copied again" 'grep -q "^\*moving *a => a2" "$T/log"'
echo z > "$T/sysroot/nested/more"; echo z2 > "$T/sysroot/rfile"
sleep 2
check "a change inside a nested subvolume stays out" '[[ ! -e $T/dst/root/nested/more ]] && synced'
check "no periodic passes needed" '! grep -q "not watched" "$T/log"'
stop

# A subvol= mount doesn't reach the top level: warn and fall back.
start "$T/log2" "$T/syshome/" "$T/dst/home2/"
check "subvol= mount: warning and periodic passes" \
  'grep -q "0 of 1 pairs" "$T/log2" && grep -q "can.t watch.*subvolid=5" "$T/log2"'
stop

[[ $fail == 0 ]] && echo "all btrfs tests passed"
exit $fail
