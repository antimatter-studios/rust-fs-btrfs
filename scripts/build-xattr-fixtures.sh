#!/usr/bin/env bash
#
# build-xattr-fixtures.sh — a filesystem carrying extended attributes,
# and a manifest of what the kernel says is on it.
#
# # Why this is its own fixture
#
# Attributes have to be *set*, which means a mount, which means this
# cannot come from build-fixtures-native.sh's mkfs-only matrix. It is
# built the same way the subvolume fixture is, by the same pattern: one
# script that CI runs directly and the macOS VM runner copies into the
# guest, so a developer's local run and the gate cannot mean different
# things.
#
# # What the manifest is for
#
# `getfattr` is the reference answer. Recording its output beside the
# image means the driver's own listing is compared against what the
# kernel reports for the same filesystem, rather than against what the
# driver believes. The comparison is what makes this an oracle rather
# than a round-trip through our own assumptions.
#
# # The shape, and why each file is there
#
#   plain.txt     four attributes, covering the value shapes that decode
#                 differently: ordinary text, a ZERO-LENGTH value (a real
#                 value, and not the same as the attribute being absent),
#                 arbitrary binary including NUL bytes, and one long
#                 enough that its item is mostly value.
#   collide.txt   TWO NAMES THAT HASH TO THE SAME KEY. Btrfs files an
#                 attribute under (ino, 24, crc32c(~1, name)), so
#                 colliding names share one item and their records are
#                 packed end to end inside it. A driver that read only
#                 the first record would return one attribute and no
#                 error, which is the failure this fixture exists to
#                 catch. The two names below were found by searching the
#                 hash; both must come back.
#   bare.txt      no attributes at all, so an empty list is distinguished
#                 from a failure to look.
#   dir/          a directory with an attribute, since directories carry
#                 them too and nothing else here would prove it.
#   dir/inner.txt an attribute in the `trusted` namespace, which only
#                 root may set. Btrfs stores the prefix as part of the
#                 name, so this must come back spelled in full — the
#                 sibling ext4 and EROFS drivers would have to expand a
#                 prefix table to say the same thing.
#
#   ./scripts/build-xattr-fixtures.sh
set -euo pipefail

OUT="${BTRFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${BTRFS_FIXTURE_SIZE:-512M}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.btrfs >/dev/null || {
    echo "mkfs.btrfs not found — install btrfs-progs" >&2
    exit 1
}
# setfattr and getfattr come from the `attr` package. Without them this
# script would build a filesystem with no attributes on it and a manifest
# saying so, and the oracle test would pass having compared nothing.
command -v setfattr >/dev/null && command -v getfattr >/dev/null || {
    echo "setfattr/getfattr not found — install attr" >&2
    exit 1
}

# The two names that share a key. crc32c(~1, name) is identical for both,
# which is 0x5bc5594f; see the header comment above for why that matters.
# Regenerating this pair means searching the same hash for another
# collision — the numbers are not arbitrary and cannot be tidied up.
COLLIDE_A="user.tag1371838"
COLLIDE_B="user.tag2000402"

mkdir -p "$OUT"
img="$OUT/btrfs-xattr.img"
manifest="$OUT/btrfs-xattr.manifest"
rm -f "$img" "$manifest"

truncate -s "$SIZE" "$img"
mkfs.btrfs -f "$img" >/dev/null

m="$(mktemp -d)"
$SUDO mount -o loop "$img" "$m"

echo "plain" | $SUDO tee "$m/plain.txt" >/dev/null
$SUDO setfattr -n user.colour -v "blue" "$m/plain.txt"
# -v "" is a zero-length value, not an unset attribute.
$SUDO setfattr -n user.empty -v "" "$m/plain.txt"
# 0x… is setfattr's hex form: NUL, high bytes, a newline — the things a
# value must survive being.
$SUDO setfattr -n user.binary -v 0x000102ff7f0a00 "$m/plain.txt"
$SUDO setfattr -n user.long -v "$(printf 'x%.0s' $(seq 1 2000))" "$m/plain.txt"

echo "collide" | $SUDO tee "$m/collide.txt" >/dev/null
$SUDO setfattr -n "$COLLIDE_A" -v "first of the pair" "$m/collide.txt"
$SUDO setfattr -n "$COLLIDE_B" -v "second of the pair" "$m/collide.txt"

echo "bare" | $SUDO tee "$m/bare.txt" >/dev/null

$SUDO mkdir -p "$m/dir"
$SUDO setfattr -n user.on-a-directory -v "yes" "$m/dir"
echo "inner" | $SUDO tee "$m/dir/inner.txt" >/dev/null
$SUDO setfattr -n trusted.root-only -v "only root may set this" "$m/dir/inner.txt"

$SUDO sync

# The reference answer, in getfattr's own words and taken while the
# filesystem is still mounted. `-e hex` so a binary value survives being
# written down; `-d -m -` so every namespace is dumped, not just `user`;
# paths relative to the mount point so the manifest does not carry a
# tempdir name that changes every run.
{
    echo "# getfattr -R -d -m - -e hex, from the mounted filesystem."
    echo "# The reference the driver's own listing is compared against."
    echo "# An attribute with a zero-length value prints as a bare name,"
    echo "# with no '=' — that is getfattr's spelling, not a truncation."
    (cd "$m" && $SUDO getfattr -R -d -m - -e hex .)
} | $SUDO tee "$manifest" >/dev/null

$SUDO umount "$m"
rmdir "$m"

btrfs inspect-internal dump-super -f "$img" > "$OUT/btrfs-xattr.superdump"

echo "BUILT  btrfs-xattr"
sed 's/^/  /' "$manifest"
