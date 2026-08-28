#!/usr/bin/env bash
#
# build-cow-fixtures.sh — one filesystem, before and after one change.
#
# The remaining piece of the write path is the recursion: recording an
# allocation modifies the extent tree, which itself lives in allocated
# blocks. Reasoning about how the kernel breaks that cycle is how a
# writer ends up implementing something plausible and wrong.
#
# So it is measured instead. This captures a WHOLE IMAGE before and
# after a single minimal metadata change, so every block the kernel
# rewrote can be identified and every item it changed can be diffed. The
# question it answers is not "what does the documentation say a
# transaction does" but "what did this one actually do".
#
# What is captured:
#
#   btrfs-cow-before.img    straight after mkfs, mounted and unmounted
#                           once so the first-mount feature write is
#                           already done and does not pollute the diff
#   btrfs-cow-control.img   mounted and unmounted again, CHANGING
#                           NOTHING
#   btrfs-cow-after.img     one `touch` and one `sync` later
#
# The control is what makes the measurement mean anything. Mounting a
# filesystem read-write commits by itself, so a before/after pair around
# a `touch` contains the touch AND whatever a bare mount cycle does.
# Without a pair that did nothing, every one of those writes would be
# attributed to creating the file.
#
# The mount/unmount before the "before" image matters. The first
# read-write mount of a filesystem made by a newer mkfs sets the
# BIG_METADATA incompat bit and initialises uuid_tree_generation, and
# without this the diff would show that alongside the change under
# study.
#
#   ./scripts/build-cow-fixtures.sh
set -euo pipefail

OUT="${BTRFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${BTRFS_FIXTURE_SIZE:-512M}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.btrfs >/dev/null || { echo "mkfs.btrfs not found" >&2; exit 1; }

mkdir -p "$OUT"
before="$OUT/btrfs-cow-before.img"
after="$OUT/btrfs-cow-after.img"
rm -f "$before" "$after"

truncate -s "$SIZE" "$before"
mkfs.btrfs -f "$before" >/dev/null

m="$(mktemp -d)"

# Settle the first-mount writes so they are not part of the diff.
$SUDO mount -o loop "$before" "$m"
$SUDO umount "$m"

control="$OUT/btrfs-cow-control.img"
rm -f "$control"
cp --sparse=always "$before" "$control"
cp --sparse=always "$before" "$after"

# The control: the same mount cycle, changing nothing.
$SUDO mount -o loop "$control" "$m"
$SUDO sync
$SUDO umount "$m"

# The change under study: one empty file, one sync. The smallest
# metadata transaction there is.
$SUDO mount -o loop "$after" "$m"
$SUDO touch "$m/one"
$SUDO sync
$SUDO umount "$m"

rmdir "$m"

echo "BUILT  before, control (no change) and after (one touch)"
for f in "$before" "$control" "$after"; do
    gen=$(od -An -tu8 -j $((65536 + 72)) -N 8 "$f" | tr -d ' ')
    root=$(od -An -tu8 -j $((65536 + 80)) -N 8 "$f" | tr -d ' ')
    used=$(od -An -tu8 -j $((65536 + 120)) -N 8 "$f" | tr -d ' ')
    printf '  %-24s generation %-4s root %-12s bytes_used %s\n' \
        "$(basename "$f")" "$gen" "$root" "$used"
done
