#!/usr/bin/env python3
"""Creates a source tree with every kind of entry fansync must handle."""
import os, socket, subprocess, sys

root = sys.argv[1]
os.makedirs(root, exist_ok=True)
J = lambda *p: os.path.join(root, *p)

def write(p, data, mode=None):
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, 'wb') as f:
        f.write(data)
    if mode is not None:
        os.chmod(p, mode)

write(J('empty'), b'')
write(J('small.txt'), b'hello\n')
write(J('exec.sh'), b'#!/bin/sh\necho hi\n', 0o755)
write(J('setgid-file'), b'x', 0o2755)
write(J('readonly'), b'ro', 0o444)
write(J('big.bin'), os.urandom(5 << 20) * 10)  # 50 MiB, above the in-place threshold later
write(J('medium.bin'), os.urandom(3 << 20))
# Sparse: data, 100 MiB hole, data, trailing hole.
with open(J('sparse.img'), 'wb') as f:
    f.write(b'A' * 4096); f.seek(100 << 20); f.write(b'B' * 8192); f.truncate(300 << 20)
# Entirely a hole.
with open(J('allhole.img'), 'wb') as f:
    f.truncate(64 << 20)
# Odd names.
write(J('dir with spaces', 'file\nwith newline'), b'nl')
write(J(b'latin1-\xe9\xe8'.decode('latin-1')), b'x')  # valid utf-8 name
os.makedirs(J('bytes'), exist_ok=True)
with open(os.path.join(os.fsencode(J('bytes')), b'non-utf8-\xff\xfe'), 'wb') as f:
    f.write(b'raw bytes name')
write(J('-dash'), b'd')
write(J('.hidden'), b'h')
# Deep nesting.
deep = J(*['d%02d' % i for i in range(40)])
write(os.path.join(deep, 'leaf'), b'leaf')
# Symlinks: relative, absolute, dangling, to a directory, loop.
os.symlink('small.txt', J('link-rel'))
os.symlink('/etc/hostname', J('link-abs'))
os.symlink('does/not/exist', J('link-dangling'))
os.symlink('dir with spaces', J('link-dir'))
os.symlink('loop-b', J('loop-a')); os.symlink('loop-a', J('loop-b'))
# Hard links, including across directories and a 3-link group.
write(J('hl', 'a'), b'linked content\n')
os.makedirs(J('hl', 'sub'), exist_ok=True)
os.link(J('hl', 'a'), J('hl', 'b'))
os.link(J('hl', 'a'), J('hl', 'sub', 'c'))
write(J('hl', 'pair1'), b'other group')
os.link(J('hl', 'pair1'), J('hl', 'sub', 'pair2'))
# FIFO and socket.
os.mkfifo(J('fifo'), 0o640)
s = socket.socket(socket.AF_UNIX); s.bind(J('sock')); s.close()
# Directory permissions and an empty directory.
os.makedirs(J('emptydir'), exist_ok=True)
os.makedirs(J('private'), exist_ok=True); write(J('private', 'secret'), b's', 0o600); os.chmod(J('private'), 0o700)
os.makedirs(J('sticky'), exist_ok=True); os.chmod(J('sticky'), 0o1777)
# Extended attributes and ACLs.
os.setxattr(J('small.txt'), 'user.comment', b'hello xattr')
os.setxattr(J('small.txt'), 'user.binary', bytes(range(256)))
os.setxattr(J('emptydir'), 'user.dirattr', b'on a dir')
os.setxattr(J('big.bin'), 'user.big', b'v' * 3000)
subprocess.run(['setfacl', '-m', f'u:{os.getuid()}:rw,g:{os.getgid()}:r', J('medium.bin')], check=True)
subprocess.run(['setfacl', '-d', '-m', f'u:{os.getuid()}:rwx', J('private')], check=True)
# Old and future timestamps, with nanoseconds.
os.utime(J('small.txt'), ns=(1, 123456789))
os.utime(J('exec.sh'), ns=(4102444800_000000001, 4102444800_000000001))
os.utime(J('link-rel'), ns=(1000_000000007, 1000_000000007), follow_symlinks=False)
