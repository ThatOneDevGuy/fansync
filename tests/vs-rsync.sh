#!/usr/bin/env bash
# Differential test: rsync and fansync, given the same source, the same
# pre-populated destination and the same filter rules, must produce the same
# destination tree.
set -euo pipefail
F=${F:-$(dirname "$0")/../target/release/fansync}
T=$(mktemp -d "${TMPDIR:-/tmp}/fansync-vs.XXXX")
trap 'rm -rf "$T"' EXIT
mk() {  # mk ROOT: source-like tree
  mkdir -p "$1"/{home/sen/workspaces/proj,home/sen/.local/share/containers/x,opt/app,var/log/journal,var/lib,etc,lost+found,keep/deep}
  echo a > "$1"/home/sen/.profile; echo w > "$1"/home/sen/workspaces/proj/f
  echo c > "$1"/home/sen/.local/share/containers/x/layer; echo o > "$1"/opt/app/bin
  echo l > "$1"/var/log/journal/j; echo v > "$1"/var/lib/db; echo e > "$1"/etc/conf
  echo k > "$1"/keep/deep/file; echo s > "$1"/swap; echo t > "$1"/cache.tmp; echo t > "$1"/etc/x.tmp
}
mk "$T/src"
# A destination with stale and extra content, some of it excluded.
mk "$T/dst0"
echo stale > "$T/dst0/etc/conf"; echo extra > "$T/dst0/etc/extra"; mkdir -p "$T/dst0/gone/sub"; echo g > "$T/dst0/gone/sub/f"
echo extra > "$T/dst0/home/sen/only-in-dst"; echo extra > "$T/dst0/opt/app/only-in-dst"
echo extra > "$T/dst0/var/log/journal/old"; echo ex > "$T/dst0/old.tmp"; mkdir -p "$T/dst0/gone2"; echo x > "$T/dst0/gone2/keep.tmp"
case_n=0; fail=0
check() {  # check RSYNC-FILTER-ARGS...
  case_n=$((case_n+1))
  rm -rf "$T/r" "$T/f"; cp -a "$T/dst0" "$T/r"; cp -a "$T/dst0" "$T/f"
  rsync -aHAXSx --numeric-ids --delete "$@" "$T/src${SLASH-/}" "$T/r/" 2>/dev/null || true
  "$F" --once -q --delete "$@" "$T/src${SLASH-/}" "$T/f/" 2>/dev/null || true
  # Directory mtimes legitimately differ where rsync couldn't remove a
  # directory, so compare everything else.
  if diff <(cd "$T/r" && find . -printf '%y %m %s %p\n' | sort) <(cd "$T/f" && find . -printf '%y %m %s %p\n' | sort) >/dev/null &&
     diff -r "$T/r" "$T/f" >/dev/null; then
    echo "ok   $case_n: $*"
  else
    echo "FAIL $case_n: $*"; fail=1
    diff <(cd "$T/r" && find . -printf '%y %m %s %p\n' | sort) <(cd "$T/f" && find . -printf '%y %m %s %p\n' | sort) | sed 's/^/     /'
  fi
}
check
check --exclude='/home/*' --exclude='/opt/*' --exclude='/var/*' --exclude=/lost+found --exclude=/swap
check --exclude='/sen/workspaces/*' --exclude='/sen/.local/share/containers/*'
check --exclude='/log/*'
check --exclude='*.tmp'
check --exclude='etc/'
check --exclude='etc'
check --include='/keep/***' --exclude='*'
check --include='/etc/' --include='/etc/conf' --exclude='*'
check --exclude='**/journal'
check --exclude='/gone'
check --exclude='sub/f'
check --exclude='/home/sen/.profile' --exclude='[a-f]*'
# Source without a trailing slash: rsync's patterns then start with its name.
mkdir -p "$T/dst0/src/etc" && echo extra > "$T/dst0/src/etc/only-in-dst"
SLASH= check --exclude='/src/etc/*'
SLASH= check --exclude='/src/home/sen/*' --exclude='etc'
SLASH= check --exclude='/src'
exit $fail
