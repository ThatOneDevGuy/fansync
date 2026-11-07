#!/usr/bin/env python3
"""Random filesystem operations on a tree, for stress-testing watch mode.

usage: fuzz.py ROOT SEED OPS
"""
import os, random, shutil, sys

root, seed, ops = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rnd = random.Random(seed)

def entries(kind=None):
    out = []
    for d, dirs, files in os.walk(root):
        for n in dirs:
            if kind in (None, 'd'):
                out.append(os.path.join(d, n))
        for n in files:
            p = os.path.join(d, n)
            if kind in (None, 'f') and (kind is None or (os.path.isfile(p) and not os.path.islink(p))):
                out.append(p)
    return out

def anydir():
    ds = [root] + entries('d')
    return rnd.choice(ds)

def name():
    return rnd.choice(['a', 'b', 'c', 'dir', 'file', 'x y', 'ü', '.h']) + str(rnd.randrange(30))

def op():
    k = rnd.randrange(16)
    try:
        if k <= 2:  # create file
            p = os.path.join(anydir(), name())
            with open(p, 'wb') as f:
                f.write(os.urandom(rnd.choice([0, 10, 5000, 200000])))
        elif k == 3:  # mkdir
            os.makedirs(os.path.join(anydir(), name(), name()), exist_ok=True)
        elif k == 4:  # append
            fs = entries('f')
            if fs:
                with open(rnd.choice(fs), 'ab') as f:
                    f.write(os.urandom(rnd.randrange(1, 3000)))
        elif k == 5:  # overwrite in the middle
            fs = entries('f')
            if fs:
                p = rnd.choice(fs)
                with open(p, 'r+b') as f:
                    f.seek(rnd.randrange(max(1, os.path.getsize(p))))
                    f.write(os.urandom(100))
        elif k == 6:  # truncate
            fs = entries('f')
            if fs:
                os.truncate(rnd.choice(fs), rnd.randrange(5000))
        elif k == 7:  # rename anything
            es = entries()
            if es:
                src = rnd.choice(es)
                dst = os.path.join(anydir(), name())
                if not dst.startswith(src + '/') and dst != src:
                    os.rename(src, dst)
        elif k == 8:  # delete file
            fs = entries('f')
            if fs:
                os.unlink(rnd.choice(fs))
        elif k == 9:  # delete tree
            ds = entries('d')
            if ds and rnd.random() < 0.3:
                shutil.rmtree(rnd.choice(ds))
        elif k == 10:  # chmod
            es = [e for e in entries() if not os.path.islink(e)]
            if es:
                os.chmod(rnd.choice(es), rnd.choice([0o644, 0o600, 0o755, 0o700, 0o640]))
        elif k == 11:  # symlink
            os.symlink(name(), os.path.join(anydir(), name()))
        elif k == 12:  # hard link
            fs = entries('f')
            if fs:
                os.link(rnd.choice(fs), os.path.join(anydir(), name()))
        elif k == 13:  # xattr
            fs = entries('f')
            if fs:
                os.setxattr(rnd.choice(fs), 'user.k' + str(rnd.randrange(3)), os.urandom(rnd.randrange(1, 50)))
        elif k == 14:  # atomic replace
            fs = entries('f')
            if fs:
                p = rnd.choice(fs)
                t = p + '.tmp'
                with open(t, 'wb') as f:
                    f.write(os.urandom(rnd.randrange(1, 9000)))
                os.rename(t, p)
        elif k == 15:  # touch with an old time
            es = entries()
            if es:
                os.utime(rnd.choice(es), ns=(rnd.randrange(10**18), rnd.randrange(10**18)), follow_symlinks=False)
    except (FileNotFoundError, FileExistsError, IsADirectoryError, NotADirectoryError, OSError):
        pass

for i in range(ops):
    op()
