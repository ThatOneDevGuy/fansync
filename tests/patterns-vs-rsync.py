#!/usr/bin/env python3
"""Differential test of filter patterns: rsync and fansync copy the same
tree of awkward names with the same --exclude, for each pattern below, and
must copy the same files. Usage: tests/patterns-vs-rsync.py [PATTERN...]
(SLASH= for a source without a trailing slash)."""
import os, subprocess, sys, tempfile, shutil
F = os.environ.get("F", os.path.join(os.path.dirname(os.path.abspath(__file__)), "../target/release/fansync"))
PATTERNS = ['[[:digit:]]*', '[[:alpha:]]*', '[[:upper:]]*', '[[:lower:]]', '[[:alnum:]]', '[[:punct:]]', '[[:space:]]', '[[:xdigit:]]*', '[![:digit:]]*', '[^[:digit:]]*', '[[:digit:][:upper:]]*', '[[:foo:]]*', '[[:digit:]-z]*', '[[:digit:]', '[x', '[[x', 'x]', '[!a-z]*', 'foo\\bar', '/foo\\bar', 'sub/foo\\bar', 'foo\\\\bar', 'foo\\*', 'foo\\b*', '*\\bar', 'foo\\', '\\x', '\\[x', 'f\\[x', 'f[[]x', '[]]', '[!]]*', '[a-]*', '[-a]*', '[\\]]', '**/foo', '**/1foo', 'sub/**', '/sub/**/foo*', '**/deep/*', 'sub/***', 'a?b', '*foo', 'foo*', '/**/foo', '**foo', '**', '***', 'foo.d/***', 'f*/***', '/sub/***', 'sub/deep/', '**/deep/', '[f]oo\\bar', 'foo[\\\\]bar', 'foo[\\\\]*', '[\\\\]x', '\\\\x', '\\\\*', '*\\\\', '[a\\-z]*', '[a\\]]*', '[[]*', '[[:digit:]]', '[[:digit:]]/*', '*/[[:digit:]]*', '[[:alpha:][:digit:]][[:digit:]]', '1*/f', '?foo', '[!-]*', '[^a-z]*', '[[:upper:]][[:lower:]]*', 'a\\ b', 'a?b', 'x\\]', 'x[]]']
names = ["1foo", "foo", "a1", "foo\\bar", "foobar", "foo*", "f[x", "[x", "x]", "a-b", "A", "b",
         ".hidden", "UP", "-", "]", "a^b", "!x", "foo\\", "\\x", "x", "fab", "z9", "Foo1", "a b",
         "sub/1foo", "sub/foo", "sub/foo\\bar", "sub/deep/foo", "sub/deep/1foo", "foo.d/inner", "1dir/f"]
pats = sys.argv[1:] or PATTERNS
root = tempfile.mkdtemp()
src = os.path.join(root, "src")
for n in names:
    p = os.path.join(src, n)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    if not os.path.exists(p):
        open(p, "w").write("x")
def listing(d):
    out = []
    for dp, ds, fs in os.walk(d):
        for n in ds + fs:
            out.append(os.path.relpath(os.path.join(dp, n), d))
    return sorted(out)
bad = 0
for pat in [p for p in pats if p]:
    r, f = os.path.join(root, "r"), os.path.join(root, "f")
    shutil.rmtree(r, ignore_errors=True); shutil.rmtree(f, ignore_errors=True); os.makedirs(r); os.makedirs(f)
    subprocess.run(["rsync", "-r", "--exclude=" + pat, src + os.environ.get("SLASH", "/"), r + "/"], capture_output=True)
    subprocess.run([F, "--once", "-q", "--exclude=" + pat, src + os.environ.get("SLASH", "/"), f + "/"], capture_output=True)
    a, b = listing(r), listing(f)
    if a != b:
        bad += 1
        print(f"FAIL {pat!r}: rsync-only {sorted(set(a)-set(b))} fansync-only {sorted(set(b)-set(a))}")
    else:
        print(f"ok   {pat!r}")
shutil.rmtree(root)
print(f"{bad} of {len([p for p in pats if p])} patterns differ")
sys.exit(1 if bad else 0)
