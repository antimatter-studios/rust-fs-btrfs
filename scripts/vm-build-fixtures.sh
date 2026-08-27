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

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

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

# ---------------------------------------------------------------------
# Populated fixtures: mounted inside the VM and filled so the fs tree
# grows past a single leaf. tests/fstree_oracle.rs fails outright if no
# fixture has a multi-level tree, so this pass is not optional cover.
# ---------------------------------------------------------------------
for spec in "${BTRFS_POPULATED[@]}"; do
    name="${spec%%:*}"; rest="${spec#*:}"
    args="${rest%%:*}"; rest="${rest#*:}"
    count="${rest%%:*}"; size="${rest##*:}"
    "$REPO/scripts/vm.sh" run "
        set -e
        cd /share
        rm -f btrfs-$name.img btrfs-$name.superdump
        truncate -s $size btrfs-$name.img
        if ! mkfs.btrfs $args -f btrfs-$name.img >/dev/null 2>&1; then
            rm -f btrfs-$name.img
            echo 'SKIP  $name (mkfs.btrfs rejected this geometry)'
            exit 0
        fi
        mnt=\$(mktemp -d)
        mount -o loop btrfs-$name.img \$mnt
        mkdir -p \$mnt/many
        seq 1 $count | xargs -P4 -I{} sh -c \"echo {} > \$mnt/many/f{}.txt\"
        sync
        umount \$mnt; rmdir \$mnt
        btrfs inspect-internal dump-super -f btrfs-$name.img > btrfs-$name.superdump
        echo 'BUILT $name ($count files)'
    "
done

echo
echo "Populated fixtures built. Now run:"
echo "  cargo test --test fstree_oracle -- --nocapture"

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
build_rich vm

# ---------------------------------------------------------------------
# The nodatacow fixture: the one shape of file Btrfs writes in place.
#
# A `chattr +C` directory makes its files NODATACOW|NODATASUM, which is
# what lets a driver overwrite their blocks without a transaction — no
# new extent, no checksum item, no tree rewrite. The ordinary file
# beside it is the control: a driver that ignored the flag and wrote in
# place regardless would corrupt it while looking correct on the other,
# so tests/write_oracle.rs requires that one to be refused.
# ---------------------------------------------------------------------
"$REPO/scripts/vm.sh" run "
    set -e
    cd /share
    img=btrfs-nodatacow.img
    rm -f \$img
    truncate -s $BTRFS_RICH_SIZE \$img
    mkfs.btrfs -f \$img >/dev/null 2>&1
    mnt=\$(mktemp -d)
    mount -o loop \$img \$mnt
    mkdir \$mnt/nc
    chattr +C \$mnt/nc
    dd if=/dev/urandom of=\$mnt/nc/inplace.bin bs=4096 count=64 status=none
    dd if=/dev/urandom of=\$mnt/cow.bin bs=4096 count=64 status=none
    sync; umount \$mnt; rmdir \$mnt
    echo 'BUILT nodatacow'
"

# ---------------------------------------------------------------------
# One fixture per compression algorithm, each with a manifest the kernel
# generated. The manifest is the whole point: it records what Linux says
# each file contains, so the driver's decoders are checked against the
# encoder that produced the bytes rather than against themselves.
# ---------------------------------------------------------------------
build_compressed() {
    local algo="$1"
    local img="btrfs-comp-$algo.img"
    local script="
        set -e
        cd \"\$SHARE_DIR\"
        rm -f $img btrfs-comp-$algo.manifest btrfs-comp-$algo.compression
        truncate -s $BTRFS_RICH_SIZE $img
        mkfs.btrfs -f $img >/dev/null 2>&1
        mnt=\$(mktemp -d)
        mount -o loop,compress=$algo $img \$mnt

        # Long enough to span many sectors, which is the only way the LZO
        # segment framing shows up at all.
        python3 -c \"print('the quick brown fox jumps over the lazy dog '*40000)\" > \$mnt/big.txt
        # Compressible but under one sector, so the single-segment case
        # is covered too.
        python3 -c \"print('ab'*200)\" > \$mnt/small.txt
        # Incompressible: must stay an ordinary extent and still read.
        dd if=/dev/urandom of=\$mnt/plain.bin bs=1M count=2 status=none
        # Tiny enough to live inline in its own item.
        echo 'inline and compressible aaaaaaaaaaaaaaaaaaaaaaaa' > \$mnt/inline.txt
        sync; umount \$mnt; rmdir \$mnt

        # What the kernel says each file holds, read back through its own
        # driver on a read-only mount so the image is not disturbed.
        mnt=\$(mktemp -d)
        mount -o ro $img \$mnt
        ( cd \$mnt
          find . -mindepth 1 -type f | sort | while read -r p; do
            printf '%s\\t%s\\t%s\\n' \"\${p#.}\" \"\$(stat -c%s \"\$p\")\" \"\$(sha256sum \"\$p\" | cut -d' ' -f1)\"
          done
        ) > btrfs-comp-$algo.manifest
        umount \$mnt; rmdir \$mnt

        # Which compression types actually ended up on disk. If this says
        # only 'none', the mount option was ignored and the fixture is
        # not testing what it claims to.
        btrfs inspect-internal dump-tree -t 5 $img 2>/dev/null | \
            grep -o 'extent compression [0-9]* ([a-z]*)' | sort -u \
            > btrfs-comp-$algo.compression

        echo \"BUILT comp-$algo (\$(wc -l < btrfs-comp-$algo.manifest) files, \$(tr '\\n' ' ' < btrfs-comp-$algo.compression))\"
    "
    SHARE_DIR=/share "$REPO/scripts/vm.sh" run "SHARE_DIR=/share; $script"
}

for algo in $BTRFS_COMPRESSION_ALGOS; do
    build_compressed "$algo"
done
