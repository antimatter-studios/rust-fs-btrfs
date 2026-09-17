#!/usr/bin/env bash
#
# build-nodatacow-fixtures.sh — one filesystem holding a checksummed
# file and a file with no checksums, so a driver's data-checksum path
# can be judged on both.
#
# # Why this is its own fixture
#
# `chattr +C` has to be applied to a directory on a *mounted*
# filesystem, so this cannot come from build-fixtures-native.sh's
# mkfs-only matrix. It is built the same way the subvolume and
# extended-attribute fixtures are: one script CI runs directly and the
# macOS VM runner copies into the guest, so a developer's local run and
# the gate cannot mean different things.
#
# This build already existed, inline in scripts/vm-build-fixtures.sh —
# which no workflow calls. The image was therefore built on a
# developer's machine and nowhere else, and every test needing it
# skipped in CI and reported success. Factoring it out is what lets the
# gate have it.
#
# # The shape, and why each file is there
#
#   nc/inplace.bin  written into a `chattr +C` directory, so the kernel
#                   marks it NODATACOW, which implies NODATASUM: it has
#                   no checksums at all. A driver that treated a missing
#                   digest as damage would refuse this file, which the
#                   kernel reads happily — the failure that is worse for
#                   a user than the one being guarded against.
#   cow.bin         an ordinary file beside it, so every sector has a
#                   digest in the csum tree. Without it a driver that
#                   simply refused every read would pass the first half.
#
# Both are 256 KiB of random bytes: several sectors each, so a
# per-sector check has more than one sector to get right, and
# incompressible, so the extents stay ordinary rather than being folded
# into something else.
#
#   ./scripts/build-nodatacow-fixtures.sh
set -euo pipefail

OUT="${BTRFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${BTRFS_FIXTURE_SIZE:-512M}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.btrfs >/dev/null || {
    echo "mkfs.btrfs not found — install btrfs-progs" >&2
    exit 1
}
# chattr comes from e2fsprogs. Without it the `nc` directory would be an
# ordinary directory, both files would be checksummed, and the half of
# the oracle that proves a NODATASUM file still reads would be comparing
# a file against itself.
command -v chattr >/dev/null || {
    echo "chattr not found — install e2fsprogs" >&2
    exit 1
}

mkdir -p "$OUT"
img="$OUT/btrfs-nodatacow.img"
rm -f "$img"

truncate -s "$SIZE" "$img"
mkfs.btrfs -f "$img" >/dev/null

m="$(mktemp -d)"
$SUDO mount -o loop "$img" "$m"

$SUDO mkdir "$m/nc"
$SUDO chattr +C "$m/nc"
$SUDO dd if=/dev/urandom of="$m/nc/inplace.bin" bs=4096 count=64 status=none
$SUDO dd if=/dev/urandom of="$m/cow.bin" bs=4096 count=64 status=none

# The flags as the kernel recorded them, printed rather than assumed.
# `C` on the file is what says the image carries the case this fixture
# exists for; a build where chattr silently did nothing would otherwise
# produce two ordinary files and an oracle that compares nothing.
$SUDO lsattr "$m/nc/inplace.bin" "$m/cow.bin"
$SUDO lsattr -d "$m/nc" | grep -q 'C' || {
    echo "the nc/ directory is not NODATACOW — chattr +C did not take" >&2
    $SUDO umount "$m"
    rmdir "$m"
    exit 1
}

$SUDO sync
$SUDO umount "$m"
rmdir "$m"

btrfs inspect-internal dump-super -f "$img" > "$OUT/btrfs-nodatacow.superdump"

echo "BUILT  btrfs-nodatacow"

# # btrfs-nodatacow-snapshot: the same file, after a snapshot (#63)
#
# A snapshot does NOT raise a data extent's reference count when the
# subvolume's tree is taller than one leaf. `btrfs_copy_root` adds a
# reference to each block the root points at, and here those are tree
# blocks, not data. So `nc/inplace.bin`'s extent still reads `refs 1`
# while the snapshot reads the same bytes. The kernel tells the two
# apart with the fs tree's `last_snapshot`: an extent from a generation
# at or before it may be shared. This image is that case. The 3000 empty
# files in `many/` are only there so the fs tree is a node before the
# snapshot is taken, and the script checks that it is.
#
# It goes in its own directory. The suites that walk every image in
# `.vm-share` assume what an ordinary volume holds, and a snapshot breaks
# that on purpose: the blocks under the shared root carry two inline
# references, a 42-byte METADATA_ITEM where every other image has 33.
mkdir -p "$OUT/snapshot"
img="$OUT/snapshot/btrfs-nodatacow-snapshot.img"
rm -f "$img"
truncate -s "$SIZE" "$img"
mkfs.btrfs -f "$img" >/dev/null
m="$(mktemp -d)"
$SUDO mount -o loop "$img" "$m"
$SUDO mkdir "$m/nc" "$m/many"
$SUDO chattr +C "$m/nc"
$SUDO dd if=/dev/urandom of="$m/nc/inplace.bin" bs=4096 count=64 status=none
(cd "$m/many" && seq -f 'f%05g' 1 3000 | $SUDO xargs touch)
$SUDO sync
$SUDO btrfs subvolume snapshot -r "$m" "$m/snap" >/dev/null
$SUDO lsattr "$m/nc/inplace.bin"
$SUDO sync
$SUDO umount "$m"
rmdir "$m"
level=$(btrfs inspect-internal dump-tree -t 5 "$img" | awk '/^leaf /{print 0; exit} /^node /{print $4; exit}')
[ "${level:-0}" -ge 1 ] || {
    echo "the fs tree is level ${level:-?}, so the snapshot raised every data reference" >&2
    exit 1
}
echo "BUILT  btrfs-nodatacow-snapshot (fs tree level $level)"
