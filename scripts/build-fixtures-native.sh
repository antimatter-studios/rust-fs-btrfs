#!/usr/bin/env bash
#
# build-fixtures-native.sh — build the oracle fixture matrix on a Linux
# host, without the VM.
#
# This is what CI runs. GitHub's Linux runners already are the oracle:
# mkfs.btrfs, `btrfs inspect-internal` and the in-kernel driver are all
# right there, so the VM (which exists only because macOS has none of
# them) would be pure overhead.
#
# Output is identical to scripts/vm-build-fixtures.sh — the same
# geometries, the same btrfs-<name>.img + btrfs-<name>.superdump pairs in
# .vm-share — so tests/oracle_vm_fixtures.rs cannot tell which builder
# produced them.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/fixture-geometries.sh
source "$REPO/scripts/fixture-geometries.sh"

SHARE="$REPO/.vm-share"
mkdir -p "$SHARE"

command -v mkfs.btrfs >/dev/null 2>&1 || {
    echo "mkfs.btrfs not found — install btrfs-progs" >&2
    exit 1
}

echo "btrfs-progs: $(btrfs --version 2>&1 | head -1)"
echo

built=()
skipped=()

for geom in "${BTRFS_GEOMETRIES[@]}"; do
    name="${geom%%:*}"
    args="${geom#*:}"
    img="$SHARE/btrfs-$name.img"
    dump="$SHARE/btrfs-$name.superdump"

    rm -f "$img" "$dump"
    truncate -s "$BTRFS_FIXTURE_SIZE" "$img"

    # shellcheck disable=SC2086  # $args is a deliberate argument list
    if mkfs.btrfs $args -f "$img" >/dev/null 2>&1; then
        # -f dumps every superblock copy, not just the primary.
        btrfs inspect-internal dump-super -f "$img" > "$dump"
        echo "BUILT $name"
        built+=("$name")
    else
        # Never drop a geometry silently: a fixture that quietly stopped
        # being generated is a hole in the gate that still reports green.
        rm -f "$img"
        echo "SKIP  $name (mkfs.btrfs rejected: ${args:-<no args>})"
        skipped+=("$name")
    fi
done

echo
echo "Built ${#built[@]} of ${#BTRFS_GEOMETRIES[@]} geometries into $SHARE"

if [ "${#skipped[@]}" -gt 0 ]; then
    echo
    echo "!!  NOT BUILT: ${skipped[*]}"
    echo "!!  Those geometries are uncovered by the oracle test in this run."
    echo "!!  Check the btrfs-progs version above before trusting a green result."
fi

# The default geometry is the floor of the gate. If even that one fails
# to build, the environment is broken and every downstream assertion is
# meaningless — say so now rather than letting the oracle test "pass"
# because it found nothing to compare.
if [ ! -f "$SHARE/btrfs-default.img" ]; then
    echo "the default geometry failed to build — aborting" >&2
    exit 1
fi
