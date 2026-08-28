#!/usr/bin/env bash
#
# build-pool-fixtures.sh — a btrfs filesystem spanning two devices.
#
# Everything else in the fixture matrix is one image. A pool is
# different in kind: a chunk stripe names the device it lives on, so
# reading one disk of a two-disk filesystem is not a partial read, it is
# a read of the wrong bytes — they parse, and on a mirrored pool they
# may even checksum.
#
# Two images are built and mkfs'd together as one filesystem, in the
# profile a mirror uses:
#
#   btrfs-pool-a.img   device 1
#   btrfs-pool-b.img   device 2   (RAID1 data and metadata)
#
# What a reader is expected to do with one of them is refuse, and
# tests/pool_oracle.rs holds it to that.
#
#   ./scripts/build-pool-fixtures.sh
set -euo pipefail

OUT="${BTRFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${BTRFS_POOL_SIZE:-512M}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"
command -v mkfs.btrfs >/dev/null || { echo "mkfs.btrfs not found" >&2; exit 1; }

mkdir -p "$OUT"
a="$OUT/btrfs-pool-a.img"
b="$OUT/btrfs-pool-b.img"
rm -f "$a" "$b"
truncate -s "$SIZE" "$a"
truncate -s "$SIZE" "$b"

# Loop devices, because mkfs.btrfs takes devices rather than files when
# building a multi-device filesystem.
la=$($SUDO losetup --find --show "$a")
lb=$($SUDO losetup --find --show "$b")
cleanup() { $SUDO losetup -d "$la" 2>/dev/null || true; $SUDO losetup -d "$lb" 2>/dev/null || true; }
trap cleanup EXIT

$SUDO mkfs.btrfs -f -d raid1 -m raid1 "$la" "$lb" >/dev/null

echo "BUILT  a two-device RAID1 filesystem"
for f in "$a" "$b"; do
    devs=$(od -An -tu8 -j $((65536 + 0x88)) -N 8 "$f" | tr -d ' ')
    echo "  $(basename "$f")  num_devices $devs"
done
