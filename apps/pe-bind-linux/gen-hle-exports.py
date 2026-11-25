#!/usr/bin/env python3
"""Regenerate data/hle-exports.tsv from victory-hle's module tables.

Each line is `<dll-glob>\t<import name>` for every export victory-hle serves
today. Three declaration styles exist in `modules/*.rs`:

* `hle_export!(Name)` — Rust name equals the imported name;
* `HleExport { name: "Name", address: ... }` — explicit name;
* `ordinal("#N", f)` — a Winsock ordinal (`#N`).

    python3 apps/pe-bind-linux/gen-hle-exports.py > apps/pe-bind-linux/data/hle-exports.tsv
"""
import pathlib
import re
import sys

SRC = pathlib.Path(__file__).resolve().parents[2] / "crates" / "victory-hle" / "src" / "modules"

dll_re = re.compile(r'dlls:\s*&\[(.*?)\]', re.S)
str_re = re.compile(r'"([^"]+)"')
patterns = [
    re.compile(r'hle_export!\(\s*([A-Za-z0-9_]+)\s*\)'),
    re.compile(r'name:\s*"([^"]+)"'),
    re.compile(r'ordinal\(\s*"(#[0-9]+)"'),
]

rows = set()
for path in sorted(SRC.glob("*.rs")):
    text = path.read_text()
    blocks = list(dll_re.finditer(text))
    for i, m in enumerate(blocks):
        dlls = str_re.findall(m.group(1))
        end = blocks[i + 1].start() if i + 1 < len(blocks) else len(text)
        body = text[m.end():end]
        for pattern in patterns:
            for name in pattern.findall(body):
                for dll in dlls:
                    rows.add((dll.lower(), name))

for dll, name in sorted(rows):
    sys.stdout.write(f"{dll}\t{name}\n")
print(f"{len(rows)} rows", file=sys.stderr)
