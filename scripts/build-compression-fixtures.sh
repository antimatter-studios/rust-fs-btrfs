#!/usr/bin/env bash
#
# build-compression-fixtures.sh — one filesystem per compression
# algorithm, each with a manifest the kernel generated.
#
# # Why this is its own script
#
# The build lived inline in scripts/vm-build-fixtures.sh, which no
# workflow calls, and build-fixtures-native.sh (the one CI runs) never
# made these images (#69). So tests/compression_oracle.rs found nothing
# on every pull request and reported success: the zlib, LZO and zstd
# decoders were checked on developers' machines only. One script that
# CI runs directly and the macOS VM runner copies into the guest, as the
# nodatacow, subvolume and xattr fixtures already are, means the two
# cannot mean different things.
#
# # The shape
#
# The manifest is the whole point: it records what Linux says each file
# contains, so the driver's decoders are checked against the encoder that
# produced the bytes rather than against themselves.
#
#   big.txt     compressible and many sectors long, the only way the LZO
#               segment framing shows up at all
#   small.txt   compressible but under one sector: the single-segment case
#   plain.bin   incompressible, so it stays an ordinary extent and the
#               plain path is checked beside the compressed one
#   inline.txt  small enough to live inline in its own item
#
#   ./scripts/build-compression-fixtures.sh [algo...]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Beside this script: in scripts/ in a checkout, and in the share
# directory when the VM runner copies both into the guest.
# shellcheck source=scripts/fixture-geometries.sh
source "$HERE/fixture-geometries.sh"

OUT="${BTRFS_FIXTURE_DIR:-$HERE/../.vm-share}"
SIZE="${BTRFS_FIXTURE_SIZE:-$BTRFS_RICH_SIZE}"
ALGOS=("$@")
[ "${#ALGOS[@]}" -gt 0 ] || read -r -a ALGOS <<< "$BTRFS_COMPRESSION_ALGOS"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

for tool in mkfs.btrfs btrfs python3 sha256sum; do
    command -v "$tool" >/dev/null || {
        echo "$tool not found" >&2
        exit 1
    }
done

mkdir -p "$OUT"

build() {
    local algo="$1"
    local img="$OUT/btrfs-comp-$algo.img"
    local manifest="$OUT/btrfs-comp-$algo.manifest"
    local record="$OUT/btrfs-comp-$algo.compression"
    rm -f "$img" "$manifest" "$record"

    truncate -s "$SIZE" "$img"
    mkfs.btrfs -f "$img" >/dev/null

    local m
    m="$(mktemp -d)"
    $SUDO mount -o "loop,compress=$algo" "$img" "$m"
    python3 -c "print('the quick brown fox jumps over the lazy dog '*40000)" |
        $SUDO tee "$m/big.txt" >/dev/null
    python3 -c "print('ab'*200)" | $SUDO tee "$m/small.txt" >/dev/null
    $SUDO dd if=/dev/urandom of="$m/plain.bin" bs=1M count=2 status=none
    echo 'inline and compressible aaaaaaaaaaaaaaaaaaaaaaaa' | $SUDO tee "$m/inline.txt" >/dev/null
    $SUDO sync
    $SUDO umount "$m"

    # What the kernel says each file holds, read back through its own
    # driver on a read-only mount so the image is not disturbed.
    $SUDO mount -o loop,ro "$img" "$m"
    ( cd "$m"
      find . -mindepth 1 -type f | sort | while read -r p; do
          printf '%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" "$(sha256sum "$p" | cut -d' ' -f1)"
      done
    ) > "$manifest"
    $SUDO umount "$m"
    rmdir "$m"

    # Which compression types actually ended up on disk. If this does not
    # name the algorithm, the mount option was ignored and the fixture is
    # not testing what it claims to, so the build fails here rather than
    # in a test that reads it as a pass.
    btrfs inspect-internal dump-tree -t 5 "$img" 2>/dev/null |
        grep -o 'extent compression [0-9]* ([a-z]*)' | sort -u > "$record"
    grep -q "($algo)" "$record" || {
        echo "btrfs-comp-$algo: no $algo-compressed extent on disk: $(tr '\n' ' ' < "$record")" >&2
        exit 1
    }

    echo "BUILT  btrfs-comp-$algo ($(wc -l < "$manifest") files, $(tr '\n' ' ' < "$record"))"
}

for algo in "${ALGOS[@]}"; do
    build "$algo"
done
