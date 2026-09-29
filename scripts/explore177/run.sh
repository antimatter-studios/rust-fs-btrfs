#!/usr/bin/env bash
# TEMPORARY exploration for #177, run by .github/workflows/explore-177.yml.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$HERE/../.."
EX="$REPO/target/release/examples/explore177"
OUT=/tmp/x177; mkdir -p $OUT

# variant NAME SIZE MKFSARGS FILES FILESIZE_KIB K
variant() {
    local name=$1 size=$2 mkfs=$3 files=$4 kib=$5 k=$6
    local img=$OUT/$name.img
    echo "::group::variant $name size=$size mkfs=[$mkfs] files=$files kib=$kib k=$k"
    rm -f $img; truncate -s $size $img
    mkfs.btrfs -q -f $mkfs $img >/dev/null
    local m; m=$(mktemp -d)
    sudo mount -o loop $img $m
    sudo bash -c '
        m=$1 files=$2 kib=$3 k=$4
        head -c $((kib * 1024)) /dev/urandom > /tmp/one
        for ((f = 0; f < files; f++)); do cp /tmp/one "$m/f$f" 2>/dev/null || break; done
        sync
        for ((f = 0; f < files; f += k)); do rm -f "$m/f$f"; done
        sync
    ' _ $m $files $kib $k
    sudo umount $m; rmdir $m
    sudo chown $(id -u) $img
    btrfs --version | head -1
    btrfs inspect-internal dump-tree -t 10 $img | grep -m3 -A1 FREE_SPACE_INFO
    python3 $HERE/analyse.py $img | tee $OUT/$name.txt | grep -v "^  leaf#" | head -60
    grep -c "^  leaf#" $OUT/$name.txt
    local cand; cand=$(grep ^CANDIDATE $OUT/$name.txt | head -1)
    if [ -n "$cand" ]; then
        set -- $cand
        cp --sparse=always $img $OUT/$name-t.img
        echo "transaction dirtying $4 in group $2"
        $EX $OUT/$name-t.img $4 | tail -40
        echo "explore177 exit ${PIPESTATUS[0]}"
        btrfs check --readonly $OUT/$name-t.img 2>&1 | tail -25
        echo "btrfs check exit ${PIPESTATUS[0]}"
    fi
    echo "::endgroup::"
}

variant mixed2g-k2 2G "-M -n 4096 -s 4096" 1300 1024 2
variant mixed2g-k3 2G "-M -n 4096 -s 4096" 1300 1024 3
variant mixed4g-k2 4G "-M -n 4096 -s 4096" 2600 1024 2
variant mixed2g-256k-k2 2G "-M -n 4096 -s 4096" 5000 256 2
variant n16k-8g-16k-k2 8G "-n 16384 -s 4096" 40000 16 2
