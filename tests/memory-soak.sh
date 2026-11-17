#!/usr/bin/env bash
# Memory soak test: fansync watches a btrfs filesystem laid out like a
# desktop backup (top level mounted read-only, one pair per subvolume) while
# files are created, deleted, renamed and hard-linked nonstop, with a full
# pass every ~36 s. Every SAMPLE seconds it prints fansync's resident memory
# and the allocator's own count of bytes in use (malloc_stats, through gdb),
# which tells a leak (in use keeps growing) from memory that was freed but
# kept (resident grows, in use doesn't). Needs real root: run it in the VM.
# Usage: tests/memory-soak.sh [MINUTES]   (HEAPTRACK=1 to record with heaptrack;
# BULK=N for N more files, a fifth of them hard-linked, as flatpak and
# container storage do; RESCAN=HOURS between full passes, default 0.01)
set -uo pipefail
D=$(cd "$(dirname "$0")" && pwd)
F=${F:-$D/../target/release/fansync}
MIN=${1:-20}
SAMPLE=${SAMPLE:-60}
T=/tmp/fansync-soak
modprobe btrfs 2>/dev/null
umount -R "$T"/* 2>/dev/null; rm -rf "$T"; mkdir -p "$T"/{top,ro,dst}
truncate -s 5G "$T.img"; mkfs.btrfs -q -f -m single "$T.img" > /dev/null
L=$(losetup -f --show "$T.img"); mount "$L" "$T/top"
mkdir "$T/top/os"; btrfs -q subvolume create "$T/top/os/root"; btrfs -q subvolume create "$T/top/os/home"
python3 - "$T/top/os" <<'PY'
import os, sys
for sub, n in (("root", 100000), ("home", 50000)):
    for i in range(n):
        d = os.path.join(sys.argv[1], sub, f"d{i // 500}", f"e{i // 50 % 10}")
        if i % 50 == 0:
            os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}"), "w") as f:
            f.write(str(i))
PY
if [[ -n ${BULK:-} ]]; then
  cc -O2 -o "$T/churn" "$D/churn.c" && "$T/churn" "$T/top/os/root/bulk" fill "$BULK"
  mkdir "$T/top/os/root/linked"
  for k in $(seq 0 $((BULK / 5000 - 1))); do cp -al "$T/top/os/root/bulk/d$k" "$T/top/os/root/linked/"; done
  echo "source: $(find "$T/top/os" -type f | wc -l) files, $(find "$T/top/os" -type f -links +1 | wc -l) of them hard-linked"
fi
mount -o ro,subvolid=5 "$L" "$T/ro"
# Small files inline in the inode, and enough inodes for BULK.
truncate -s 6G "$T.dst.img"; mkfs.ext4 -q -F -O inline_data -N $((${BULK:-0} * 2 + 400000)) "$T.dst.img"
mount -o loop "$T.dst.img" "$T/dst"
cat > "$T/cfg.toml" <<EOF
delete = true
rescan = ${RESCAN:-0.01}
link_delay = 30
settle = 0.5
max_delay = 3
[[pair]]
src = "$T/ro/os/root/"
dst = "$T/dst/root/"
[[pair]]
src = "$T/ro/os/home/"
dst = "$T/dst/home/"
EOF
run=("$F" --config "$T/cfg.toml")
[[ -n ${HEAPTRACK:-} ]] && run=(heaptrack -o "$T/heaptrack" "${run[@]}")
"${run[@]}" > "$T/log" 2>&1 &
for _ in $(seq 3600); do grep -q "watching for changes" "$T/log" && break; sleep 1; done
pid=$(pgrep -n -x fansync)
echo "fansync pid $pid: $(grep -c 'full pass' "$T/log") full passes so far"

churn() { # what a desktop does: caches, logs, renames, saves, hard links
  local r=$T/top/os/root h=$T/top/os/home i=0
  while :; do
    i=$((i + 1))
    mkdir -p "$h/cache/$i"; for j in $(seq 100); do echo "$j" > "$h/cache/$i/c$j"; done
    rm -rf "$h/cache/$((i - 3))"
    echo "line $i" >> "$r/var.log"
    mv "$r/d1" "$r/d1x" && mv "$r/d1x" "$r/d1"
    ln "$h/d2/e0/f1000" "$h/d2/e0/link$i" 2>/dev/null; rm -f "$h/d2/e0/link$((i - 5))"
    head -c 1M /dev/urandom > "$r/blob$((i % 5))"
    echo "$i" > "$r/d3/e1/.save.tmp" && mv "$r/d3/e1/.save.tmp" "$r/d3/e1/saved"
    mkdir -p "$r/tree$i/a/b"; echo x > "$r/tree$i/a/b/f"; rm -rf "$r/tree$((i - 2))"
    sleep 0.2
  done
}
churn & cpid=$!

mb() { awk -v k="$1:" '$1 == k { printf "%d", $2 / 1024 }' "/proc/$pid/status"; }
heap() { # allocator totals (MB): in use, and held from the system
  [[ -n ${HEAPTRACK:-} ]] && { echo "- -"; return; }
  timeout 120 gdb -p "$pid" -batch -ex 'call (void)malloc_stats()' > /dev/null 2>&1
  awk '/^Total/ { t = 1 } t && /system bytes/ { s = $4 } t && /in use bytes/ { u = $5 } END { printf "%d %d", u / 1048576, s / 1048576 }' "$T/log"
}
printf "%6s %8s %8s %10s %10s %7s\n" minute rss_mb anon_mb inuse_mb held_mb passes
end=$((SECONDS + MIN * 60)); start=$SECONDS
while ((SECONDS < end)) && kill -0 "$pid" 2>/dev/null; do
  read -r inuse held < <(heap)
  printf "%6d %8s %8s %10s %10s %7s\n" $(((SECONDS - start) / 60)) "$(mb VmRSS)" "$(mb RssAnon)" "$inuse" "$held" "$(grep -c 'full pass' "$T/log")"
  sleep "$SAMPLE"
done
kill "$cpid"; wait "$cpid" 2>/dev/null
if [[ -z ${HEAPTRACK:-} ]]; then
  echo "peak resident: $(mb VmHWM) MB"
  timeout 120 gdb -p "$pid" -batch -ex 'call (int)malloc_trim(0)' > /dev/null 2>&1
  echo "after malloc_trim (freed memory given back): rss $(mb VmRSS) MB, anon $(mb RssAnon) MB"
fi
grep -E "^[0-9:]+ (error|warning)" "$T/log" | sort | uniq -c | sort -rn | head -5
kill -INT "$pid"; for _ in $(seq 60); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
if [[ -n ${HEAPTRACK:-} ]]; then
  sleep 5
  heaptrack_print -f "$T"/heaptrack*.zst 2>/dev/null | sed -n '/^PEAK MEMORY CONSUMERS/,/^MOST TEMPORARY/p' | head -60
  heaptrack_print -f "$T"/heaptrack*.zst 2>/dev/null | grep -E "^(total runtime|calls to allocation|temporary memory|peak heap memory|peak RSS|total memory leaked)"
fi
umount -R "$T"/* 2>/dev/null; losetup -d "$L" 2>/dev/null
