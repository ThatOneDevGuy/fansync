# fansync

Keeps directory trees mirrored. It does one rsync-style pass, then copies
each change within seconds of it happening, using fanotify. It's built for
always-current backups of large volumes, where rerunning rsync every few
minutes means re-checking millions of files to find a handful of changes.

It copies like `rsync -aHAXSx --numeric-ids` (add `--delete` to mirror
deletions):

- regular files, directories, symlinks, hard links, device files, FIFOs and sockets
- permissions including setuid/setgid/sticky, numeric owner and group, and
  nanosecond mtimes
- extended attributes in every namespace, which covers POSIX ACLs, SELinux
  labels and file capabilities
- sparse files stay sparse
- each source stays on its own filesystem and mount: mount points are copied
  as empty directories, like `-x`. Unlike rsync, this includes bind mounts of
  the same filesystem, which `-x` can't tell apart because they keep the
  device number.
- rsync filter rules: `--exclude`/`--include` with the same pattern syntax,
  and excluded destination files are never deleted

## Usage

```sh
cargo build --release
sudo install target/release/fansync /usr/local/bin/

# One pair, rsync style: initial pass, then keep watching (Ctrl-C to stop).
sudo fansync --delete --exclude='/tmp/*' /data/ /mnt/backup/data/

# Just one pass, like rsync.
sudo fansync --once --delete --progress /data/ /mnt/backup/data/

# What would change?
sudo fansync --dry-run --delete /data/ /mnt/backup/data/

# Several pairs from a config file.
sudo fansync --config backup.toml
```

As with rsync, a trailing slash on the source copies its contents into the
destination; without one, the directory itself is copied (`DST/NAME`). Only the
last component of the destination is created, so an unmounted backup drive
makes fansync fail instead of filling the filesystem below the mount point. The
rsync flags `-aHAXSx --numeric-ids` and `--info=progress2` are accepted and
ignored, because that behaviour is always on.

### Config file

```toml
delete = true           # mirror deletions
rescan = 6              # hours between full safety passes (0 = never)
settle = 2              # seconds of quiet before syncing a burst of changes
max_delay = 30          # ...but never wait longer than this
interval = 10           # minutes between passes for sources that can't be watched
link_delay = 300        # backstop: full pass for hard links whose other names weren't found
inplace_min = "32M"     # update existing files at least this big in place
status_file = "/run/backup.status"
exit_on_error = true    # stop when the destination fails, instead of retrying
exclude = ["*.swp"]     # applied to every pair, before the pair's own rules

[[pair]]
name = "system"
src = "/"
dst = "/mnt/backup/root/"
exclude = ["/home/*", "/var/*", "/tmp/*", "/lost+found", "/swap"]

[[pair]]
name = "var"
src = "/var/"
dst = "/mnt/backup/root/var/"
rules = ["+ /log/keep-this", "- /log/*"]   # ordered "+ "/"- " rules, like rsync --filter
```

Within a pair, rules apply in this order: `rules`, then `include`, then
`exclude`, then `exclude_from` (a file of rules). The first match wins.

`examples/sync-to-renegade.sh` is the drive-sync script rewritten to generate
this config from its step list and run fansync. `examples/watch-sync.sh` is its
monitor, reading the status file instead of looking for rsync processes.

## How it stays fast

**Watching instead of rescanning.** A single `FAN_MARK_FILESYSTEM` mark covers
a whole filesystem, so it costs nothing per directory, unlike inotify, which
needs a watch for each one. Events carry the parent directory's file handle and
the entry name (`FAN_REPORT_DFID_NAME`). fansync turns handles into paths with
`open_by_handle_at`, checks the path really leads to that directory, and syncs
just the changed entries. Changes are batched until things settle for `settle`
seconds (at most `max_delay`).

**rsync's techniques, kept:**

- the quick check: equal size and mtime means equal content
- deletions in a directory happen before transfers into it
- a file that already exists in the destination is written aside and renamed
  into place, so the old copy survives a failed update
- excluded destination files are protected from deletion, so a parent-pair
  exclude (`/home/*`) keeps `--delete` away from what a child pair (`/home`)
  copies

**…and improved on:**

- The tree walk runs in parallel (`-j`) and works relative to directory file
  descriptors, using `getdents64` with large buffers and `fstatat`. Paths are
  never re-resolved from the root and symlinks are never followed.
- Data is copied inside the kernel: `copy_file_range` on the same filesystem,
  which btrfs/XFS turn into reflinks and NFS into server-side copies, and
  `sendfile` across filesystems. Data never passes through user space.
- Holes are found with `SEEK_DATA`/`SEEK_HOLE` instead of by scanning data for
  zeros, so a 1 TB sparse image with 1 GB of data costs 1 GB.
- Existing files of at least `inplace_min` are updated in place: both copies
  are read and only the 32 KiB blocks that differ are written. rsync's
  local-copy default rewrites the whole file. This means far fewer writes for
  VM images, databases and growing logs, and on a copy-on-write destination
  (btrfs) unchanged extents stay shared with snapshots.
- Renames are mirrored: renaming a 100 GB directory renames it in the
  destination instead of copying it again (needs `FAN_RENAME`, Linux 5.17+).
- New files are written with `O_TMPFILE`, so a crash never leaves temporary
  files behind.
- Big files' destination pages are dropped from the page cache once written,
  so a backup doesn't flush the running system's cache. `--drop-cache` does the
  same for source pages.

On a 9.6 GB, 205k-entry tree (warm cache, NVMe):

| | rsync -aHAXSx | fansync |
|---|---|---|
| initial copy, same disk, incl. `sync` | 20.8 s | 2.5 s |
| initial copy, other filesystem | 23.2 s | 1.2 s |
| pass with nothing changed | 1.4–2.3 s | 0.28 s |
| 1 byte changed in a 2 GB file | 3.5 s, rewrites 2 GB | 1.4 s, writes 32 KiB |
| a change while watching | (next pass) | synced ~2 s later, no scan |

### Safety nets

- **Periodic full passes** (`rescan`, every 6 h by default) catch anything
  events can't report. Writes through `mmap` only show up when the file is
  closed (`FAN_CLOSE_WRITE`).
- **Queue overflow:** without CAP_SYS_ADMIN the event queue is limited. If it
  overflows, a full pass runs.
- **Busy large files:** a file that took T to copy isn't copied again for
  10×T, so a VM image that is written constantly gets at most a ~10% duty
  cycle.
- **Files changed mid-copy:** these are queued again. The destination's mtime
  is the one from before the copy, so a later pass also notices.
- **Hard links while watching:** hard links are equal names for one inode,
  and nothing records an inode's names, so fansync keeps its own records:
  - every multiply-linked file's names
  - for every single-linked file, the directory it's in (about 20–30 bytes
    per file)

  When `ln a b` makes a second name, the first is found through those
  records and `b` is linked in the backup right away. Writes go to the
  inode, so a change made through one name updates the backup under all of
  them, even if that name is removed or replaced before it's synced. On a
  host, fansync learns of every link-count change from fanotify's nameless
  inode events, opens the file by handle, and syncs all its names.

  If a name still can't be found (it's outside the pair, or under the
  container fallback below), a full pass runs `link_delay` seconds later.
  Until then the new name is a separate copy with the same content.
- **Unmounted source or destination:** if either filesystem changes while
  running (for example an unmount), passes fail instead of mirroring an empty
  mount point or writing elsewhere. They're retried every `interval` minutes
  (or fansync exits, with `--exit-on-error`). With `--delete`, a pass is also
  refused when the source is empty but the destination isn't.
- **Mounts inside the destination:** nothing mounted inside the destination
  is ever deleted into. That includes bind mounts of the same filesystem,
  through which rsync `-x --delete` deletes files that live elsewhere.
- **Overlapping trees:** a destination inside its source must be excluded,
  and a source inside its destination is refused.
- **A user-writable destination:** the destination mirrors owners, so users
  can write into their own parts of it. Every operation there works
  relative to directory descriptors and never follows a symlink, so swapping
  a file for a symlink can't redirect a write or a chmod (`fchmodat2` with
  `AT_SYMLINK_NOFOLLOW`).
- **Renames:** these are only mirrored with `--delete`. Without it, the old
  name stays, as with rsync.
- **A failing destination drive:** I/O errors in the destination, or a
  destination that is full or has gone read-only (as btrfs and ext4 do after a
  device failure), stop the pass at the first error instead of failing file
  by file. It's retried after `interval` minutes. A drive that needs a power
  cycle won't recover by itself, though, so `--exit-on-error` (or
  `exit_on_error = true`) makes fansync exit instead, with code 11, the reason
  in the log, and `state='failed'` plus `error` in the status file. Errors
  only show up when fansync next writes, so a drive that fails while nothing
  changes is noticed with the next change.
- **Errors in the source:** files vanishing during a pass are normal on a
  live system and are only counted. A file that can't be read (a bad sector)
  fails on its own; the rest of the pass goes on. When an in-kernel copy
  fails, fansync redoes that chunk through user space to tell which drive
  failed.

## Requirements

- Linux 5.9+ (`FAN_REPORT_DFID_NAME`); 5.17+ to mirror renames. Run as root:
  filesystem marks need CAP_SYS_ADMIN, and owners, devices and `trusted.*`
  xattrs need root too.
- The source filesystem must support file handles. ext4, XFS, btrfs and
  tmpfs do. A source that can't be watched gets a full pass every `interval`
  minutes instead, and a warning says so.
- **btrfs subvolumes:** since Linux 6.8 the kernel only allows filesystem
  marks in the top-level subvolume, because each subvolume has its own fsid.
  One mark still reports changes in every subvolume, so for a subvolume
  source fansync places the mark on the nearest directory above it that's in
  the top level and on the same mount. That works when sources are given
  through a mount of the top level (`mount -o subvolid=5`), such as
  `/mnt/top/@home/`. A source on a `subvol=` mount (like `/home` on most
  installs) can't reach the top level, so it gets periodic passes. Nested
  subvolumes inside a source are copied as empty directories, like mount
  points.
- Where `open_by_handle_at` is blocked (container seccomp profiles, or no
  CAP_DAC_READ_SEARCH), fansync indexes the source directories' handles once
  and keeps the index current from events. On Linux 5.17+ the index is exact.
  It costs some memory per directory. This mode can't open a file by handle,
  so a hard link that is created, used and removed within about 0.1 s is
  only caught by the `link_delay` pass.

## Monitoring

`--status-file FILE` keeps a file of shell `KEY='value'` lines up to date:
`state` (`initial-sync`, `watching`, `syncing`, `done`, `stopped`, `failed`),
`pair`, `pid`, `started`, `updated`, `last_full_pass`, `last_change`,
`pending`, `copied_files`, `written_bytes`, `deleted`, `errors`, and `error`
(why fansync stopped, with `--exit-on-error`).

`-i` prints rsync-style itemized changes (`>f.st...... path`), plus
`*moving     old => new` for mirrored renames. `-v` prints changed paths and
`-vv` adds debug output.

Exit codes follow rsync: 0 means success, 23 means some files failed, 11
means a pass failed as a whole (the destination failed, or a pass couldn't
start), 20 means interrupted, and 1 is a fatal error. In watch mode, fansync
only exits on its own with `--exit-on-error` (code 11).

## Tests

```sh
cargo test                        # filter matching, path ordering, parsing
tests/vs-rsync.sh                 # same results as rsync for 16 filter/delete cases
unshare -Urm tests/watch-test.sh  # watch mode, 26 scenarios on a tmpfs source
unshare -Urm tests/fuzz-watch.sh  # random concurrent changes while watching, then an exact comparison
unshare -Urm tests/safety-test.sh # unmounted/empty source, mounts in the destination, ...
tests/mktree.py DIR && fansync --once SRC/ DST/ && tests/treecmp.py SRC DST
tests/vm.sh tests/vm-tests.sh     # all of it as real root in a QEMU VM, plus ext4, btrfs subvolumes
                                  # and failing drives (device-mapper)
```
