#!/usr/bin/env bash
# Random operations against a watched tree, then an exact comparison.
# Run as: unshare -Urm tests/fuzz-watch.sh [ROUNDS] [OPS]
set -uo pipefail
D=$(cd "$(dirname "$0")" && pwd)
F=${F:-$D/../target/release/fansync}
T=/tmp/fansync-fuzz
rounds=${1:-5} ops=${2:-3000} fail=0
for r in $(seq "$rounds"); do
  rm -rf "$T"; mkdir -p "$T"/{src,dst}
  if [[ ${SRCFS:-tmpfs} == ext4 ]]; then
    truncate -s 3G "$T.img" && mkfs.ext4 -q -F "$T.img" && mount -o loop "$T.img" "$T/src"
  else
    mount -t tmpfs -o size=2g src "$T/src"
  fi
  python3 "$D/fuzz.py" "$T/src" "$r" 300
  "$F" --delete --settle 0.2 --max-delay 2 --link-delay 600 ${FANSYNC_ARGS:-} "$T/src/" "$T/dst/" >"$T/out" 2>"$T/log" &
  pid=$!
  for _ in $(seq 100); do grep -q "watching for changes" "$T/log" && break; sleep 0.1; done
  # Two writers at once, to get events interleaving.
  python3 "$D/fuzz.py" "$T/src" $((r * 1000 + 1)) "$ops" & python3 "$D/fuzz.py" "$T/src" $((r * 1000 + 2)) "$ops"; wait %2 2>/dev/null
  sleep 4
  if out=$(python3 "$D/treecmp.py" "$T/src" "$T/dst"); then
    echo "ok   round $r: $(echo "$out" | tail -1)"
  else
    echo "FAIL round $r"; echo "$out" | head -20 | sed 's/^/     /'; fail=1
    grep -iE "error|warning" "$T/log" | head -5 | sed 's/^/     log: /'
    cp "$T/log" "${KEEP:-/tmp}/fuzz-fail-$r.log"; cp "$T/out" "${KEEP:-/tmp}/fuzz-fail-$r.out"
    [[ ${KEEP_TREES:-} ]] && rm -rf "/tmp/fuzz-fail-$r".{src,dst} && cp -a "$T/src" "/tmp/fuzz-fail-$r.src" && cp -a "$T/dst" "/tmp/fuzz-fail-$r.dst"
  fi
  kill $pid; wait $pid 2>/dev/null
  umount "$T/src"
done
exit $fail
