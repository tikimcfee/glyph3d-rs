#!/usr/bin/env python3
"""Compare two harness outputs line by line and sort the differences.

A difference whose results are NaN on both sides is a NaN-payload difference
(Rust leaves NaN bits unspecified); everything else is a value difference and
is printed with both lines. Sweep chunk lines carry two digests, NaNs folded
to one pattern and raw: only the first differing is a value difference.
"""
import sys
from collections import Counter


def isnan(h):
    v = int(h, 16)
    if len(h) <= 8:
        return (v >> 23) & 0xFF == 0xFF and v & 0x7FFFFF
    return (v >> 52) & 0x7FF == 0x7FF and v & 0xFFFFFFFFFFFFF


a = open(sys.argv[1]).read().splitlines()
b = open(sys.argv[2]).read().splitlines()
if len(a) != len(b):
    sys.exit(f"line counts differ: {len(a)} vs {len(b)}")
nan, val, ex = Counter(), Counter(), {}
for x, y in zip(a, b):
    if x == y or x.startswith("lines:"):
        continue
    k = x.split()[0]
    rx, ry = x.split("=> ")[-1].split(), y.split("=> ")[-1].split()
    if k.startswith("sweep."):
        same_values = rx[0] == ry[0]
    else:
        same_values = (k.startswith(("f32.", "f64.")) and len(rx) == 1 and isnan(rx[0]) and isnan(ry[0])
                       and not k.endswith("bits_rt"))
    if same_values:
        nan[k] += 1
    else:
        val[k] += 1
        ex.setdefault(k, []).append((x, y))
print(f"{len(a)} lines; NaN-payload-only differences: {sum(nan.values())}; value differences: {sum(val.values())}")
for k, n in sorted(nan.items()):
    print(f"  nan  {k}: {n}")
for k, v in ex.items():
    print(f"  VALUE {k}: {len(v)}")
    for x, y in v[: int(sys.argv[3]) if len(sys.argv) > 3 else 3]:
        print(f"    A {x}\n    B {y}")
