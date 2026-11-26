#!/usr/bin/env python3
"""Breaks a captured termway output stream down by command type (stdlib only).
usage: analyze_stream.py DUMP [seconds]"""
import re
import sys
from collections import Counter

data = open(sys.argv[1], "rb").read()
secs = float(sys.argv[2]) if len(sys.argv) > 2 else 1.0
cnt, size = Counter(), Counter()
pos = 0
pat = re.compile(rb"\x1b_G([^;\x1b]*)(?:;[^\x1b]*)?\x1b\\|\x1b\[[0-9;?]*[ -/]*[@-~]|[^\x1b]+", re.S)
for m in pat.finditer(data):
    tok = m.group(0)
    if tok.startswith(b"\x1b_G"):
        keys = dict(kv.split(b"=", 1) for kv in m.group(1).split(b",") if b"=" in kv)
        a = keys.get(b"a", b"t").decode()
        if a == "d":
            kind = "delete d=" + keys.get(b"d", b"a").decode()
        elif a in "tT" or (b"m" in keys and len(keys) <= 2):
            kind = "transmit (incl. chunks)"
        else:
            kind = "place a=" + a
    elif tok.startswith(b"\x1b["):
        kind = "CSI " + chr(tok[-1])
    else:
        kind = "text"
    cnt[kind] += 1
    size[kind] += len(tok)
total = sum(size.values())
print(f"total {total} bytes ({total / secs / 1024:.0f} KiB/s over {secs:.1f} s)")
for k, v in size.most_common():
    print(f"  {k:28s} {v:>10d} B {v * 100 / total:5.1f}%  n={cnt[k]:>8d}  avg {v / cnt[k]:.1f} B")
