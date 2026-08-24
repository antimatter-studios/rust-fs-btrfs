#!/usr/bin/env bash
#
# vm-build-fixtures.sh — build real Btrfs filesystems in the oracle VM.
#
# For each geometry: create a sparse image, format it with the canonical
# mkfs.btrfs, and dump the superblock with
# `btrfs inspect-internal dump-super -f`. Both land in .vm-share, where
# tests/oracle_vm_fixtures.rs picks them up and requires this driver to
# agree with the reference dump field by field.
#
# The geometry list lives in scripts/fixture-geometries.sh so the VM gate
# and the CI gate build exactly the same matrix.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/fixture-geometries.sh
source "$REPO/scripts/fixture-geometries.sh"

"$REPO/scripts/vm.sh" up

skipped=()

# The loop is driven from the host so the geometry list stays in one
# place and shell quoting stays sane.
for geom in "${BTRFS_GEOMETRIES[@]}"; do
    name="${geom%%:*}"
    args="${geom#*:}"
    out="$("$REPO/scripts/vm.sh" run "
        set -e
        cd /share
        rm -f btrfs-$name.img btrfs-$name.superdump
        truncate -s $BTRFS_FIXTURE_SIZE btrfs-$name.img
        if mkfs.btrfs $args -f btrfs-$name.img >/dev/null 2>&1; then
            # -f dumps every superblock copy, not just the primary: the
            # mirrors at 64M/256G must agree, and a reader that only ever
            # looks at offset 65536 never notices when they do not.
            btrfs inspect-internal dump-super -f btrfs-$name.img > btrfs-$name.superdump
            echo 'BUILT $name'
        else
            # Not every geometry is accepted by every btrfs-progs version
            # (checksum algorithms in particular arrived over several
            # releases). Skipping loudly beats a silently missing fixture.
            rm -f btrfs-$name.img
            echo 'SKIP $name (mkfs.btrfs rejected: $args)'
        fi
    ")"
    echo "$out"
    case "$out" in
        *SKIP*) skipped+=("$name") ;;
    esac
done

echo
echo "Fixtures in $REPO/.vm-share:"
echo "  images:     $(ls -1 "$REPO/.vm-share"/btrfs-*.img 2>/dev/null | wc -l | tr -d ' ')"
echo "  superdumps: $(ls -1 "$REPO/.vm-share"/btrfs-*.superdump 2>/dev/null | wc -l | tr -d ' ')"

if [ "${#skipped[@]}" -gt 0 ]; then
    echo
    echo "!!  ${#skipped[@]} geometry/geometries were NOT built: ${skipped[*]}"
    echo "!!  The oracle test will not cover them. Check the btrfs-progs"
    echo "!!  version in the guest (btrfs --version) before trusting a"
    echo "!!  green run."
fi

echo
echo "Now run: cargo test --test oracle_vm_fixtures -- --nocapture"
