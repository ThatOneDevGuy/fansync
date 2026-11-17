#!/usr/bin/env bash
# A sustained burst of file events while fansync is often busy with full
# passes (a 200k-file tree, a full pass every ~18 s), with the event queue
# unlimited as it is for root. Native writers rewrite their files nonstop
# (MODE=unique: create and delete temp files instead). Checks that fansync's
# memory stays bounded, that changes keep being synced during the burst, and
# that the destination matches afterwards. Needs real root: run it in the VM.
# BIG=MB writes a file that big as the burst starts, so the main thread is
# busy copying it while events pour in; FILES sets the tree size.
# Usage: tests/event-burst-test.sh [SECONDS] [WRITERS]
set -uo pipefail
D=$(cd "$(dirname "$0")" && pwd)
F=${F:-$D/../target/release/fansync}
SECS=${1:-60}
W=${2:-8}
T=/tmp/fansync-burst
fail=0
umount -R "$T"/* 2>/dev/null; rm -rf "$T"; mkdir -p "$T"/{src,dst}
mount -t tmpfs -o size=3g s "$T/src"; mkdir "$T/src/steady"
cc -O2 -o "$T/churn" "$D/churn.c" || exit 1
"$T/churn" "$T/src/bulk" fill "${FILES:-200000}"
"$F" --delete --settle 0.5 --max-delay 2 --rescan 0.005 "$T/src/" "$T/dst/" > "$T/log" 2>&1 & pid=$!
for _ in $(seq 600); do grep -q "watching for changes" "$T/log" && break; sleep 0.5; done
[[ -n ${BIG:-} ]] && dd if=/dev/zero of="$T/src/big.bin" bs=1M count="$BIG" status=none
writers=()
for w in $(seq "$W"); do "$T/churn" "$T/src/w$w" "$SECS" ${MODE:-} & writers+=($!); done
# One file changed every 5 s during the burst: did it get synced 4 s later?
lag=0
peak=0
for s in $(seq 0 5 "$SECS"); do
  echo "$s" > "$T/src/steady/tick"
  sleep 4
  rss=$(($(awk '/^VmRSS/ { print $2 }' "/proc/$pid/status") / 1024))
  ((rss > peak)) && peak=$rss
  got=$(cat "$T/dst/steady/tick" 2>/dev/null)
  [[ $got == "$s" ]] || lag=$((lag + 1))
  printf "t=%3ds rss=%5d MB  passes=%-3s tick sent %s, backup has %s\n" "$s" "$rss" "$(grep -c 'full pass' "$T/log")" "$s" "${got:-none}"
  sleep 1
done
wait "${writers[@]}" 2>/dev/null
for _ in $(seq 120); do
  python3 "$D/treecmp.py" "$T/src" "$T/dst" > /dev/null 2>&1 && break; sleep 1
done
echo "peak rss $peak MB; ticks not synced within 4 s: $lag of $((SECS / 5 + 1))"
if python3 "$D/treecmp.py" "$T/src" "$T/dst" > /dev/null; then echo "ok   destination matches after the burst"; else echo "FAIL destination differs"; fail=1; fi
grep -E "overflow|piled up|many changes|error" "$T/log" | sed 's/^[0-9:]* //' | sort | uniq -c | head -5
kill "$pid"; wait "$pid" 2>/dev/null
umount "$T/src"
exit $fail
