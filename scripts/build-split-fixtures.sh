#!/usr/bin/env bash
#
# build-split-fixtures.sh — one leaf split, caught either side.
#
# `src/leaf_edit.rs` refuses an item that will not fit rather than
# splitting the leaf, because where the kernel puts the boundary is a
# policy. Measuring the leaves of an existing filesystem showed the
# median is 91-98% FULL, so it is plainly not "half" — but a
# distribution says what the results look like, not what the rule is.
#
# This catches the event itself: the image immediately before a leaf
# split and immediately after, so the one leaf that became two can be
# compared with the one it came from.
#
# # How the moment is found
#
# By watching the fs tree's leaf count. Files are added one at a time,
# each committed, and after each the tree is counted. When the count
# goes up, the previous image is the "before" and the current one is the
# "after".
#
# The filesystem is made with the SMALLEST nodesize btrfs supports, so a
# leaf fills in a few dozen files rather than a few thousand. Each step
# unmounts, because the on-disk image of a mounted filesystem lags what
# is in memory and copying one would capture neither state.
#
# What is captured:
#
#   btrfs-split-before.img   the last image with N leaves
#   btrfs-split-after.img    the first image with N+1
#   btrfs-split.txt          how many files, and the leaf counts
#
#   ./scripts/build-split-fixtures.sh
set -euo pipefail

OUT="${BTRFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${BTRFS_FIXTURE_SIZE:-512M}"
MAX="${BTRFS_SPLIT_MAX:-400}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.btrfs >/dev/null || { echo "mkfs.btrfs not found" >&2; exit 1; }

mkdir -p "$OUT"
work="$OUT/btrfs-split-work.img"
SUFFIX="${BTRFS_SPLIT_SUFFIX:-}"
before="$OUT/btrfs-split${SUFFIX}-before.img"
after="$OUT/btrfs-split${SUFFIX}-after.img"
rm -f "$work" "$before" "$after" "$OUT/btrfs-split${SUFFIX}.txt"

truncate -s "$SIZE" "$work"
# The smallest nodesize btrfs accepts, so a leaf fills quickly.
mkfs.btrfs -n 4096 -f "$work" >/dev/null

m="$(mktemp -d)"

# How many leaves the fs tree has. Counted from the tree dump, which is
# the reference tool's own view rather than this driver's.
leaf_count() {
    $SUDO btrfs inspect-internal dump-tree -t 5 "$1" 2>/dev/null \
        | grep -c '^leaf ' || true
}

# Settle the first-mount writes before anything is measured.
$SUDO mount -o loop "$work" "$m"
$SUDO umount "$m"

prev="$OUT/btrfs-split-prev.img"
cp --sparse=always "$work" "$prev"
last=$(leaf_count "$work")
echo "start: $last leaf/leaves in the fs tree"

split_at=""
for i in $(seq 1 "$MAX"); do
    cp --sparse=always "$work" "$prev"

    $SUDO mount -o loop "$work" "$m"
    # BTRFS_SPLIT_VARY makes the items WILDLY different sizes, by
    # alternating a near-maximum filename with a one-character one. That
    # is the experiment that separates the two candidate rules: a split
    # at half the ITEM COUNT does not care about sizes, and a split at
    # half the BYTES lands somewhere else entirely once the items are
    # uneven. With every item the same size the two agree and the
    # measurement says nothing.
    if [ "${BTRFS_SPLIT_VARY:-0}" = "1" ] && [ $((i % 2)) -eq 0 ]; then
        name=$(printf 'x%.0s' $(seq 1 200))-$i
    else
        name="f$i"
    fi
    $SUDO tee "$m/$name" >/dev/null <<< "$i"
    $SUDO sync
    $SUDO umount "$m"

    now=$(leaf_count "$work")
    if [ "$now" -gt "$last" ]; then
        split_at="$i"
        cp --sparse=always "$prev" "$before"
        cp --sparse=always "$work" "$after"
        {
            echo "files_before_split=$((i - 1))"
            echo "leaves_before=$last"
            echo "leaves_after=$now"
            echo "nodesize=4096"
        } > "$OUT/btrfs-split${SUFFIX}.txt"
        echo "SPLIT  at file $i: $last leaves -> $now"
        break
    fi
    last="$now"
done

rmdir "$m"
rm -f "$prev" "$work"

if [ -z "$split_at" ]; then
    echo "no split after $MAX files — raise BTRFS_SPLIT_MAX" >&2
    exit 1
fi

echo "BUILT  btrfs-split${SUFFIX}-before.img and btrfs-split${SUFFIX}-after.img"
