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

# ---------------------------------------------------------------------
# Populated fixtures: mounted and filled so the fs tree grows past a
# single leaf. Without at least one of these, internal-node parsing and
# multi-level descent are exercised only by hand-built blocks.
# Requires root for the loop mount, which CI has.
# ---------------------------------------------------------------------
for spec in "${BTRFS_POPULATED[@]}"; do
    name="${spec%%:*}"; rest="${spec#*:}"
    args="${rest%%:*}"; rest="${rest#*:}"
    count="${rest%%:*}"; size="${rest##*:}"
    img="$SHARE/btrfs-$name.img"
    dump="$SHARE/btrfs-$name.superdump"

    rm -f "$img" "$dump"
    truncate -s "$size" "$img"

    # shellcheck disable=SC2086  # $args is a deliberate argument list
    if ! mkfs.btrfs $args -f "$img" >/dev/null 2>&1; then
        rm -f "$img"
        echo "SKIP  $name (mkfs.btrfs rejected: $args)"
        skipped+=("$name")
        continue
    fi

    mnt="$(mktemp -d)"
    if ! sudo mount -o loop "$img" "$mnt" 2>/dev/null; then
        rmdir "$mnt"; rm -f "$img"
        echo "SKIP  $name (could not loop-mount to populate it)"
        skipped+=("$name")
        continue
    fi
    sudo mkdir -p "$mnt/many"
    # xargs -P keeps this to a few seconds rather than a few minutes.
    seq 1 "$count" | sudo xargs -P4 -I{} sh -c "echo {} > $mnt/many/f{}.txt"
    sync
    sudo umount "$mnt"; rmdir "$mnt"

    btrfs inspect-internal dump-super -f "$img" > "$dump"
    echo "BUILT $name ($count files)"
    built+=("$name")
done

echo
echo "Built ${#built[@]} of $(( ${#BTRFS_GEOMETRIES[@]} + ${#BTRFS_POPULATED[@]} )) fixtures into $SHARE"

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

# ---------------------------------------------------------------------
# The rich fixture: a compressing mount over a varied tree. See
# fixture-geometries.sh for why each file is there.
# ---------------------------------------------------------------------
build_rich() {
    local run="$1"   # how to execute a shell snippet: "local" or "vm"
    local img="btrfs-$BTRFS_RICH_NAME.img"
    local script="
        set -e
        cd \"\$SHARE_DIR\"
        rm -f $img btrfs-$BTRFS_RICH_NAME.superdump
        truncate -s $BTRFS_RICH_SIZE $img
        mkfs.btrfs -f $img >/dev/null 2>&1
        mnt=\$(mktemp -d)
        mount -o loop,$BTRFS_RICH_MOUNT_OPTS $img \$mnt
        python3 -c \"print('the quick brown fox jumps over the lazy dog '*20000)\" > \$mnt/compressed.txt
        dd if=/dev/urandom of=\$mnt/plain.bin bs=1M count=2 status=none
        echo 'small inline' > \$mnt/inline.txt
        truncate -s 8M \$mnt/sparse.bin
        ln -s inline.txt \$mnt/link-short
        mkdir -p \$mnt/sub/nested && echo nested > \$mnt/sub/nested/file.txt
        sync; umount \$mnt; rmdir \$mnt
        btrfs inspect-internal dump-super -f $img > btrfs-$BTRFS_RICH_NAME.superdump
        echo 'BUILT $BTRFS_RICH_NAME (compressing mount)'
    "
    if [ "$run" = "vm" ]; then
        SHARE_DIR=/share "$REPO/scripts/vm.sh" run "SHARE_DIR=/share; $script"
    else
        SHARE_DIR="$SHARE" sudo -E bash -c "SHARE_DIR=\"$SHARE\"; $script"
    fi
}
build_rich local
