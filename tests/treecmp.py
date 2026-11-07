#!/usr/bin/env python3
"""Compares two directory trees the way a backup should match its source:
types, permissions, owners, mtimes (ns), contents, symlink targets, device
numbers, xattrs/ACLs, hard-link structure and holes. Prints differences and
exits 1 if there are any.

usage: treecmp.py SRC DST [--exclude-root NAME ...]
"""
import hashlib, os, stat, sys

def xattrs(p):
    try:
        names = os.listxattr(p, follow_symlinks=False)
    except OSError:
        return {}
    out = {}
    for n in names:
        try:
            out[n] = os.getxattr(p, n, follow_symlinks=False)
        except OSError:
            pass
    return out

def digest(p):
    h = hashlib.sha256()
    with open(p, 'rb') as f:
        while b := f.read(1 << 20):
            h.update(b)
    return h.hexdigest()

def walk(root, skip):
    out = {}
    for dirpath, dirs, files in os.walk(root):
        rel_dir = os.path.relpath(dirpath, root)
        if rel_dir == '.':
            dirs[:] = [d for d in dirs if d not in skip]
            files = [f for f in files if f not in skip]
        for n in dirs + files:
            p = os.path.join(dirpath, n)
            out[os.path.normpath(os.path.join(rel_dir, n))] = p
    return out

def main():
    src, dst = sys.argv[1], sys.argv[2]
    skip = set(sys.argv[4::2]) if len(sys.argv) > 3 else set()
    a, b = walk(src, skip), walk(dst, skip)
    diffs = []
    for k in sorted(set(a) | set(b)):
        if k not in b:
            diffs.append(f"missing in dst: {k}"); continue
        if k not in a:
            diffs.append(f"extra in dst: {k}"); continue
        sa, sb = os.lstat(a[k]), os.lstat(b[k])
        if stat.S_IFMT(sa.st_mode) != stat.S_IFMT(sb.st_mode):
            diffs.append(f"type differs: {k}"); continue
        for f in ('st_mode', 'st_uid', 'st_gid'):
            if getattr(sa, f) != getattr(sb, f):
                diffs.append(f"{f} differs: {k}: {getattr(sa, f):o} vs {getattr(sb, f):o}")
        if sa.st_mtime_ns != sb.st_mtime_ns:
            diffs.append(f"mtime differs: {k}: {sa.st_mtime_ns} vs {sb.st_mtime_ns}")
        if xattrs(a[k]) != xattrs(b[k]):
            diffs.append(f"xattrs differ: {k}: {xattrs(a[k])} vs {xattrs(b[k])}")
        if stat.S_ISLNK(sa.st_mode) and os.readlink(a[k]) != os.readlink(b[k]):
            diffs.append(f"symlink target differs: {k}")
        if stat.S_ISCHR(sa.st_mode) or stat.S_ISBLK(sa.st_mode):
            if sa.st_rdev != sb.st_rdev:
                diffs.append(f"rdev differs: {k}")
        if stat.S_ISREG(sa.st_mode):
            if sa.st_size != sb.st_size:
                diffs.append(f"size differs: {k}: {sa.st_size} vs {sb.st_size}")
            elif digest(a[k]) != digest(b[k]):
                diffs.append(f"content differs: {k}")
            # Holes: the copy may not use much more space than the source.
            if sb.st_blocks > sa.st_blocks + 64 and sa.st_blocks * 512 < sa.st_size:
                diffs.append(f"sparseness lost: {k}: {sa.st_blocks} vs {sb.st_blocks} blocks")
    # Hard-link structure: paths sharing an inode in src must share one in dst.
    def groups(t):
        g = {}
        for k, p in t.items():
            s = os.lstat(p)
            if stat.S_ISREG(s.st_mode) and s.st_nlink > 1:
                g.setdefault((s.st_dev, s.st_ino), set()).add(k)
        return sorted(sorted(v) for v in g.values() if len(v) > 1)
    ga = groups({k: v for k, v in a.items()})
    gb = groups({k: v for k, v in b.items() if k in a})
    if ga != gb:
        diffs.append(f"hard links differ:\n  src {ga}\n  dst {gb}")
    for d in diffs:
        print(d)
    print(f"{len(a)} entries compared, {len(diffs)} differences")
    sys.exit(1 if diffs else 0)

main()
