#!/usr/bin/env bash
#
# build-commit-fixtures.sh — the superblock before and after one commit.
#
# The superblock is the commit point. Everything a transaction writes is
# invisible until the superblock names the new root, and a writer that
# gets one field of it wrong produces a filesystem the kernel either
# refuses or, worse, mounts against the wrong tree.
#
# `docs/transaction-format.md` lists which fields move and when. This
# captures a real before and a real after so the list can be checked
# against a filesystem rather than trusted, and so a writer can be
# required to turn the first into the second.
#
# # Why several commits rather than one
#
# The backup slot index is `(generation - 1) mod 4`, so a single commit
# exercises one slot out of four and a writer that hardcoded a slot
# would pass. Six commits are captured, which wraps the ring and then
# some.
#
# # What is captured
#
#   btrfs-commit-<n>.super   the primary superblock, straight after the
#                            n-th commit
#
# The first is the state `mkfs` left; each subsequent one is one `touch`
# and one `sync` later. So consecutive files are a before/after pair,
# and there are six of them.
#
#   ./scripts/build-commit-fixtures.sh
set -euo pipefail

OUT="${BTRFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${BTRFS_FIXTURE_SIZE:-512M}"
COMMITS="${BTRFS_COMMITS:-6}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.btrfs >/dev/null || { echo "mkfs.btrfs not found" >&2; exit 1; }

mkdir -p "$OUT"
img="$OUT/btrfs-commit.img"
rm -f "$img" "$OUT"/btrfs-commit-*.super

truncate -s "$SIZE" "$img"
mkfs.btrfs -f "$img" >/dev/null

# The superblock as mkfs left it: the "before" of the first commit.
#
# At 64 KiB, not at the start of the device. Offset 0 is empty, and
# dumping it gives 4096 zero bytes that read as a generation of 0 and a
# root of 0 — which looks like a filesystem rather than like a mistake.
dd if="$img" of="$OUT/btrfs-commit-0.super" bs=4096 skip=16 count=1 status=none

m="$(mktemp -d)"
for n in $(seq 1 "$COMMITS"); do
    $SUDO mount -o loop "$img" "$m"
    # One file per commit, so each transaction has something to write
    # and the fs tree genuinely changes.
    echo "commit $n" | $SUDO tee "$m/file-$n.txt" >/dev/null
    $SUDO sync
    # Unmounted rather than only synced: a mounted filesystem's
    # superblock on disk lags what is in memory, and the point here is
    # what a committed superblock looks like.
    $SUDO umount "$m"

    dd if="$img" of="$OUT/btrfs-commit-$n.super" bs=4096 skip=16 count=1 status=none
done
rmdir "$m"

echo "BUILT  $((COMMITS + 1)) superblocks, $COMMITS commits apart"
for n in $(seq 0 "$COMMITS"); do
    f="$OUT/btrfs-commit-$n.super"
    # generation is at 0x048, little-endian.
    gen=$(od -An -tu8 -j 72 -N 8 "$f" | tr -d ' ')
    root=$(od -An -tu8 -j 80 -N 8 "$f" | tr -d ' ')
    printf '  %s  generation %-4s root %s\n' "$(basename "$f")" "$gen" "$root"
done
