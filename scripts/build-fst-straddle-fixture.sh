#!/usr/bin/env bash
#
# build-fst-straddle-fixture.sh — a metadata block group whose free-space
# tree records span two leaves (#177).
#
# The free-space tree files a group's FREE_SPACE_INFO and its extents in key
# order, and a leaf can end anywhere among them. The driver's rewrite read a
# group's records only from the leaf holding its INFO item.
#
# The case is built with the kernel, but its fragmentation is chosen rather
# than left to metadata churn, which fragments a group into thousands of runs
# and so into bitmaps. The volume is mixed-bg with 4 KiB nodes, so data and
# metadata share block groups and a leaf holds ~160 records. It is filled
# with 1 MiB files, each its own data extent, and every k-th file is deleted:
# each deletion frees exactly one run. (Punching holes in one preallocated
# file does not work: btrfs frees an extent only when nothing references any
# of it.) In a 218 MiB group that is under the kernel's bitmap threshold of
# ~300 extents, while the groups together overflow a leaf several times. Each
# stride is tried until `scripts/fst-straddle.py`, reading btrfs-progs' own
# dump, finds a metadata group, recorded as extents, that straddles a leaf and
# holds a tree block. If none does, this fails rather than leaving a fixture
# that no longer holds the case.
#
# The image goes to .vm-share/fst/, out of the suites that walk every image
# in .vm-share.
#
#   sudo ./scripts/build-fst-straddle-fixture.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${BTRFS_FIXTURE_DIR:-$HERE/../.vm-share}/fst"
SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"
command -v mkfs.btrfs >/dev/null || { echo "mkfs.btrfs not found — install btrfs-progs" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 not found" >&2; exit 1; }

mkdir -p "$OUT"
img="$OUT/btrfs-fst-straddle.img"

# delete every k-th file
for k in 2 3 4 5; do
    rm -f "$img"
    truncate -s 2G "$img"
    mkfs.btrfs -q -f -O mixed-bg -n 4096 -s 4096 "$img" >/dev/null
    m="$(mktemp -d)"
    $SUDO mount -o loop "$img" "$m"
    $SUDO bash -c '
        m=$1 k=$2
        head -c $((1024 * 1024)) /dev/urandom > "$m/.one"
        for ((f = 0; f < 1300; f++)); do
            cp "$m/.one" "$m/f$f" 2>/dev/null || break
        done
        rm -f "$m/.one"
        sync
        for ((f = 0; f < 1300; f += k)); do rm -f "$m/f$f"; done
        sync
    ' _ "$m" "$k"
    $SUDO umount "$m"
    rmdir "$m"
    if python3 "$HERE/fst-straddle.py" "$img"; then
        echo "BUILT  btrfs-fst-straddle (every ${k}th 1 MiB file deleted)"
        exit 0
    fi
    echo "every ${k}th file deleted: no metadata group straddles a leaf; trying the next" >&2
done
echo "no spacing produced a metadata group whose free-space records straddle a leaf" >&2
btrfs inspect-internal dump-tree -t 10 "$img" | grep -E "^leaf|FREE_SPACE_INFO" -A1 | head -80 >&2
exit 1
