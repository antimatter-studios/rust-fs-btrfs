#!/usr/bin/env python3
"""TEMPORARY exploration for #177. Reads btrfs-progs dumps of IMAGE and
prints every block group's free-space-tree shape, then candidate lines
`CANDIDATE <group start> <length> <fs-tree block>` for metadata groups in
extent form whose records span more than one leaf."""

import re
import subprocess
import sys

image = sys.argv[1]


def dump(*a):
    return subprocess.run(
        ["btrfs", "inspect-internal", "dump-tree", *a, image],
        check=True,
        capture_output=True,
        text=True,
    ).stdout


bg = {}
lines = dump("-t", "extent").splitlines()
for i, line in enumerate(lines):
    m = re.search(r"key \((\d+) BLOCK_GROUP_ITEM (\d+)\)", line)
    if m and i + 1 < len(lines):
        f = re.search(r"flags (\S+)", lines[i + 1])
        u = re.search(r"used (\d+)", lines[i + 1])
        bg[int(m.group(1))] = (
            int(m.group(2)),
            f.group(1) if f else "?",
            int(u.group(1)) if u else -1,
        )

leaf = -1
leafaddr = []
leafbitmap = []
groups = {}
order = []
lines = dump("-t", "10").splitlines()
for i, line in enumerate(lines):
    m = re.match(r"^leaf (\d+) items (\d+) free space (\d+)", line)
    if m:
        leaf += 1
        leafaddr.append((int(m.group(1)), int(m.group(2)), int(m.group(3))))
        leafbitmap.append(False)
        continue
    m = re.search(
        r"key \((\d+) (FREE_SPACE_INFO|FREE_SPACE_EXTENT|FREE_SPACE_BITMAP) (\d+)\)",
        line,
    )
    if not m:
        continue
    objectid, kind, offset = int(m.group(1)), m.group(2), int(m.group(3))
    if kind == "FREE_SPACE_BITMAP":
        leafbitmap[leaf] = True
    if kind == "FREE_SPACE_INFO":
        fl = re.search(r"extent count (\d+) flags (\d+)", lines[i + 1])
        groups[objectid] = [offset, int(fl.group(2)), [leaf], 0, int(fl.group(1))]
        order.append(objectid)
        continue
    for start in reversed(order):
        if start <= objectid < start + groups[start][0]:
            if leaf not in groups[start][2]:
                groups[start][2].append(leaf)
            groups[start][3] += 1
            break

print("FST leaves:", len(leafaddr))
for n, (a, items, free) in enumerate(leafaddr):
    print(f"  leaf#{n} at {a} items {items} free {free} bitmap={leafbitmap[n]}")

fs_blocks = [
    (int(m.group(2)), m.group(1))
    for m in re.finditer(r"^(leaf|node) (\d+) ", dump("-t", "5"), re.MULTILINE)
]
print("fs tree blocks:", len(fs_blocks), "root", fs_blocks[0] if fs_blocks else None)

for start in order:
    length, flags, leaves, records, count = groups[start]
    b = bg.get(start, (None, "NO-BG-ITEM", -1))
    inside = [a for a, k in fs_blocks if start <= a < start + length and k == "leaf"]
    print(
        f"group {start}+{length} {b[1]} used {b[2]} fstflags {flags} count {count} "
        f"records {records} leaves {leaves} fs-leaves-inside {len(inside)}"
    )
    if "METADATA" in b[1] and flags == 0 and len(leaves) > 1 and inside:
        print(f"CANDIDATE {start} {length} {inside[len(inside) // 2]}")
