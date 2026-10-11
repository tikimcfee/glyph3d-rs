#!/usr/bin/env python3
"""Format `ast-grep scan --rule tools/ast-grep/outline.yml --json=stream` as
an outline: one line per item, `start-end` (1-based, inclusive) then the
item's first line, indented as it is in the file. Reads stdin.

Run through the pixi task, which pipes the scan into this:
    pixi run outline native/src/layout_hyper/page.rs
"""
import json
import sys

for raw in sys.stdin:
    match = json.loads(raw)
    span = match["range"]
    start = span["start"]["line"] + 1
    end = span["end"]["line"] + 1
    head = match["lines"].splitlines()[0].strip()
    indent = " " * span["start"]["column"]
    print(f"{start:>5}-{end:<5} {indent}{head}")
