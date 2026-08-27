#!/usr/bin/env bash
#
# build-subvol-fixtures.sh — a filesystem with subvolumes and snapshots,
# and a manifest of what is in it.
#
# Every fixture so far has exactly one subvolume: the default fs tree,
# objectid 5. That is the only shape the driver has ever been asked to
# read, and it is not the shape a real Btrfs filesystem has — subvolumes
# are how people use it, and a snapshot is how they back it up.
#
# # What the manifest is for
#
# `btrfs subvolume list` is the reference answer: it names every
# subvolume, gives its id and its path, and says which are snapshots.
# Recording it beside the image means the driver's own enumeration can
# be compared against what btrfs-progs reports for the same filesystem,
# rather than against what the driver believes.
#
# The generation and the parent are recorded too. A snapshot shares its
# parent's tree blocks at the moment it is taken, so the two roots point
# at the same bytenr until one of them is written to — which is worth
# being able to see, because a driver that treats them as independent
# copies will read the right bytes for the wrong reason.
#
# # The shape
#
#   top        the default subvolume, with files of its own
#   sub        a subvolume beside it
#   sub/inner  a subvolume nested inside that one
#   snap       a snapshot of `sub`, taken before `sub` is written to again
#   rosnap     a read-only snapshot of `sub`
#
# `sub` gains a file AFTER `snap` is taken, so the two are genuinely
# divergent rather than identical — a driver that resolved a snapshot to
# its parent's current tree would read the extra file and pass anything
# that only counted names.
#
#   ./scripts/build-subvol-fixtures.sh
set -euo pipefail

OUT="${BTRFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${BTRFS_FIXTURE_SIZE:-512M}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.btrfs >/dev/null || {
    echo "mkfs.btrfs not found — install btrfs-progs" >&2
    exit 1
}

mkdir -p "$OUT"
img="$OUT/btrfs-subvol.img"
manifest="$OUT/btrfs-subvol.manifest"
rm -f "$img" "$manifest"

truncate -s "$SIZE" "$img"
mkfs.btrfs -f "$img" >/dev/null

m="$(mktemp -d)"
$SUDO mount -o loop "$img" "$m"

# The default subvolume, which is the only one every other fixture has.
$SUDO mkdir -p "$m/top"
echo "in the default subvolume" | $SUDO tee "$m/top/a.txt" >/dev/null

# A subvolume beside it, and one nested inside that.
$SUDO btrfs subvolume create "$m/sub" >/dev/null
echo "in sub" | $SUDO tee "$m/sub/b.txt" >/dev/null
$SUDO btrfs subvolume create "$m/sub/inner" >/dev/null
echo "in sub/inner" | $SUDO tee "$m/sub/inner/c.txt" >/dev/null
$SUDO sync

# Snapshots of `sub`, taken before it diverges.
$SUDO btrfs subvolume snapshot "$m/sub" "$m/snap" >/dev/null
$SUDO btrfs subvolume snapshot -r "$m/sub" "$m/rosnap" >/dev/null
$SUDO sync

# Now make `sub` differ from its snapshots, so a driver that resolved a
# snapshot to its parent's *current* tree would read a file that is not
# supposed to be there.
echo "added after the snapshot" | $SUDO tee "$m/sub/after.txt" >/dev/null
$SUDO sync

# The reference answer, recorded before unmounting because
# `btrfs subvolume list` needs a mounted filesystem.
{
    echo "# btrfs subvolume list -pcgu, taken from the mounted filesystem."
    echo "# Columns as btrfs-progs printed them; this is the reference the"
    echo "# driver's own enumeration is compared against."
    $SUDO btrfs subvolume list -pcgu "$m"
    echo "# --- what each subvolume holds, one line per path"
    for s in top sub sub/inner snap rosnap; do
        [ -e "$m/$s" ] || continue
        printf 'contains %s:' "$s"
        $SUDO find "$m/$s" -maxdepth 1 -type f -printf ' %f' 2>/dev/null || true
        echo
    done
} | $SUDO tee "$manifest" >/dev/null

$SUDO umount "$m"
rmdir "$m"

btrfs inspect-internal dump-super -f "$img" > "$OUT/btrfs-subvol.superdump"

echo "BUILT  btrfs-subvol"
sed 's/^/  /' "$manifest"
