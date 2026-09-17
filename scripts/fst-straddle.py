#!/usr/bin/env python3
"""Find a metadata block group whose free-space-tree records span two leaves
and which holds a tree block.

Reads `btrfs inspect-internal dump-tree -t 10` and `-t extent` for IMAGE and
prints, for each such group, `<group start> <group length> <records> <leaves>`.
Exits 1 if there is none, so a fixture that stopped holding the case says so.
Only groups recorded as extents count: a group the kernel converted to bitmaps
(FREE_SPACE_INFO flags 1) is a different shape, which the driver refuses.
"""
import re
import subprocess
import sys

image = sys.argv[1]


def dump(tree):
    return subprocess.run(
        ["btrfs", "inspect-internal", "dump-tree", "-t", tree, image],
        check=True, capture_output=True, text=True,
    ).stdout


def dump_all():
    return subprocess.run(
        ["btrfs", "inspect-internal", "dump-tree", image],
        check=True, capture_output=True, text=True,
    ).stdout


metadata = set()
lines = dump("extent").splitlines()
for i, line in enumerate(lines):
    m = re.search(r"key \((\d+) BLOCK_GROUP_ITEM (\d+)\)", line)
    if m and i + 1 < len(lines) and "METADATA" in lines[i + 1]:
        metadata.add(int(m.group(1)))

leaf = -1
groups = {}  # start -> [length, flags, set of leaves, record count]
order = []
lines = dump("10").splitlines()
for i, line in enumerate(lines):
    if re.match(r"^leaf \d+ items", line):
        leaf += 1
        continue
    m = re.search(r"key \((\d+) (FREE_SPACE_INFO|FREE_SPACE_EXTENT|FREE_SPACE_BITMAP) (\d+)\)", line)
    if not m:
        continue
    objectid, kind, offset = int(m.group(1)), m.group(2), int(m.group(3))
    if kind == "FREE_SPACE_INFO":
        flags = int(re.search(r"flags (\d+)", lines[i + 1]).group(1))
        groups[objectid] = [offset, flags, {leaf}, 0]
        order.append(objectid)
        continue
    for start in reversed(order):
        if start <= objectid < start + groups[start][0]:
            groups[start][2].add(leaf)
            groups[start][3] += 1
            break

# Every tree block, so a group can be required to hold one a test can dirty.
blocks = [int(m.group(1)) for m in re.finditer(r"^(?:leaf|node) (\d+) ", dump_all(), re.M)]

found = False
for start in order:
    length, flags, leaves, records = groups[start]
    holds_a_block = any(start <= b < start + length for b in blocks)
    if start in metadata and flags == 0 and len(leaves) > 1 and holds_a_block:
        print(start, length, records, len(leaves))
        found = True
if not found:
    for start in order:
        length, flags, leaves, records = groups[start]
        print(f"group {start}+{length}: metadata={start in metadata} flags={flags} "
              f"records={records} leaves={len(leaves)}", file=sys.stderr)
sys.exit(0 if found else 1)
