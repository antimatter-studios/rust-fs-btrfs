#!/usr/bin/env bash
#
# guest-build-images.sh <output-dir> <target>... — GUEST side: runs as
# root inside the fs-linux-test-harness VM (Debian), started by
# test-disks/build-fixtures.sh through `vm.sh run`.
#
# ONE FILE, BECAUSE THE RECIPES ARE ONE THING. This replaces nine
# scripts: scripts/build-fixtures-native.sh (which CI ran on the runner,
# with sudo), the eight scripts/build-*-fixtures.sh it could not cover,
# scripts/fixture-geometries.sh, and the eight scripts/vm-build-*.sh
# wrappers that ran the same recipes through this repository's own VM.
# Two builders meant two chances to build a different filesystem, and
# that happened: the compression and nodatacow images existed only on
# developers' machines, so their oracles found nothing in CI and
# reported success (#69).
#
# EVERY FIXTURE HERE NEEDS THE REAL KERNEL, and most of them need a
# mount: subvolumes and snapshots, extended attributes, `chattr +C`, a
# compressing mount, a leaf split caught in the act, a two-device pool.
# None of that can happen on a host that is not Linux, and none of it
# should happen on a CI runner, where `sudo mount -o loop` is a
# privilege this suite should not need and a macOS developer cannot
# have. It happens in the guest, which is the one place with a kernel
# we chose.
#
# Every image is built on the guest's own disk and copied to
# <output-dir> (on the share) only once it is complete and unmounted, so
# a failure half way never leaves a truncated image where the host picks
# fixtures up.
#
# DETERMINISM. What `mkfs.btrfs` writes carries a fresh UUID per build
# and the kernel stamps generation and mount times as it goes, so two
# builds are identical in STRUCTURE AND CONTENT rather than byte for
# byte. Tests compare against the superblock dump and the manifests
# taken from the same build, never across builds.
set -euo pipefail

[ $# -ge 2 ] || { echo "usage: guest-build-images.sh <output-dir> <target>..." >&2; exit 2; }
OUT="$1"
shift

TARGETS="geometry populated rich compression subvol xattr acl nodatacow commit cow split pool dirtylog cli features scrub seed squota encoded"

# ARGUMENTS FIRST, ENVIRONMENT SECOND. A misspelt target is the caller's
# mistake and should be named as one wherever it is made; the root and
# tool checks below describe the guest, and reporting those first would
# tell someone who typed `nodatcow` on a workstation that they need a
# Debian VM.
for target in "$@"; do
    case " $TARGETS " in
        *" $target "*) ;;
        *)
            echo "guest-build-images.sh: unknown target '$target'" >&2
            echo "  one of: $TARGETS" >&2
            exit 2
            ;;
    esac
done

[ "$(id -u)" -eq 0 ] || { echo "guest-build-images.sh runs as root in the guest." >&2; exit 1; }

for tool in mkfs.btrfs btrfs setfattr getfattr setfacl getfacl chattr lsattr python3 sha256sum losetup; do
    command -v "$tool" >/dev/null || {
        echo "guest-build-images.sh: $tool is not in the guest." >&2
        echo "  It is installed by scripts/vm-setup.sh; 'chore vm:provision' applies it again." >&2
        exit 1
    }
done

WORK="$(mktemp -d /var/tmp/btrfs-fixtures.XXXXXX)"
MNT="$WORK/mnt"
mkdir -p "$MNT" "$OUT"
cleanup() {
    mountpoint -q "$MNT" && umount "$MNT" || true
    rm -rf "$WORK"
}
trap cleanup EXIT

# ---------------------------------------------------------------------
# THE GEOMETRY MATRIX — the one list the fixtures are built from.
#
# Each entry is "<name>:<mkfs.btrfs args>". The name becomes the fixture
# filename, so keep it filesystem-safe and stable — the oracle tests
# report failures by it.
#
# Geometries are chosen to move the fields most likely to be misread:
# node size changes every b-tree item offset, the checksum algorithm
# changes both the csum_type field and the width of every csum in the
# superblock and tree blocks, the profile flags change the chunk-tree
# layout, and mixed block groups fold data and metadata into one block
# group type.
# ---------------------------------------------------------------------
GEOMETRIES=(
    "default:"
    "node4k:-n 4096"
    "node16k:-n 16384"
    "csum-crc32c:--csum crc32c"
    "csum-xxhash:--csum xxhash"
    "csum-sha256:--csum sha256"
    "csum-blake2:--csum blake2"
    "single:-d single -m single"
    "dup:-d dup -m dup"
    "mixed:-M"
)

# Every geometry fixture is this size. Large enough to clear
# mkfs.btrfs's minimum device size with room for several block groups,
# small enough that ten of them are cheap to build and to copy out.
FIXTURE_SIZE=400M

# Populated fixtures. The geometries above are all freshly-made
# filesystems, and every tree on one of those is a single leaf. That
# leaves internal-node parsing, the KeyPtr layout and the descent loop
# itself exercised only by hand-built blocks — which this project does
# not count as validated. These are mounted and filled with enough files
# to push the fs tree above level 0 (both reach level 2).
# "<name>:<mkfs.btrfs args>:<file count>:<image size>".
POPULATED=(
    "deep4k:-n 4096:20000:2G"
    "deep16k:-n 16384:60000:2G"
)

# The compression algorithms btrfs defines. All three are decoded by
# entirely different code and only one of them can be got right by
# accident: zstd and zlib are ordinary streams of their format; LZO is
# wrapped in framing btrfs invented, and that framing only becomes
# visible in a file long enough to span several sectors.
COMPRESSION_ALGOS="zlib lzo zstd"

# The two commit/COW geometries. The default one, and a SHA-256 volume
# with DUP profiles — a different checksum width and a different chunk
# layout, which is what catches a writer that hardcoded either.
GEOMETRY_VARIANTS=(
    ":--csum crc32c"
    "-sha256-dup:--csum sha256 -d dup -m dup"
)

# ---------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------

# publish <file>... — move finished artefacts into the output directory,
# each through a .partial name so the host never sees half a file.
publish() {
    local f base
    for f in "$@"; do
        base="$(basename "$f")"
        cp --sparse=always "$f" "$OUT/$base.partial"
        mv -f "$OUT/$base.partial" "$OUT/$base"
        rm -f "$f"
    done
}

# dump_super <image> — the superblock dump beside the image. `-f` dumps
# every copy, not just the primary, which is what the mirror tests read.
dump_super() {
    btrfs inspect-internal dump-super -f "$1" > "${1%.img}.superdump"
}

note() { echo "[guest] $*"; }

# The features Debian's btrfs-progs cannot format (#270) are formatted by
# the static release build scripts/vm-setup.sh installs beside it, called
# by path. Everything else keeps the distribution's tools.
STATIC_PROGS=/opt/btrfs-progs-static

# fs_tree_level <image> — the level of the fs tree's root, 0 for a leaf.
#
# THROUGH A FILE, NOT A PIPE. `awk ... exit` closes its input on the
# first matching line, the dump of a twenty-thousand-file tree is many
# megabytes, and the writer then takes SIGPIPE — which `set -o pipefail`
# turns into a failed build with status 141 and nothing to say for
# itself. Measured here, on the deep4k fixture.
fs_tree_level() {
    btrfs inspect-internal dump-tree -t 5 "$1" > "$WORK/tree-dump.txt"
    awk '/^leaf /{print 0; exit} /^node /{print $4; exit}' "$WORK/tree-dump.txt"
    rm -f "$WORK/tree-dump.txt"
}

# ---------------------------------------------------------------------
# geometry — one freshly-made filesystem per geometry, plus its dump.
#
# NOTHING IS SKIPPED. A geometry mkfs.btrfs rejects fails this build:
# a fixture that quietly stopped being generated is a hole in the gate
# that still reports green, and the suites that read it used to take
# their absent-fixture path and pass (#140).
# ---------------------------------------------------------------------
build_geometry() {
    local geom name args img
    for geom in "${GEOMETRIES[@]}"; do
        name="${geom%%:*}"
        args="${geom#*:}"
        img="$WORK/btrfs-$name.img"
        rm -f "$img"
        truncate -s "$FIXTURE_SIZE" "$img"
        # shellcheck disable=SC2086  # $args is a deliberate argument list
        mkfs.btrfs $args -f "$img" >/dev/null || {
            echo "guest-build-images: mkfs.btrfs refused the '$name' geometry (${args:-no args})" >&2
            echo "  A geometry that cannot be built is a hole in the gate, not a fixture to skip." >&2
            exit 1
        }
        dump_super "$img"
        publish "$img" "${img%.img}.superdump"
        note "built btrfs-$name"
    done
}

# ---------------------------------------------------------------------
# populated — mounted and filled until the fs tree is more than a leaf.
# ---------------------------------------------------------------------
build_populated() {
    local spec name rest args count size img level
    for spec in "${POPULATED[@]}"; do
        name="${spec%%:*}"; rest="${spec#*:}"
        args="${rest%%:*}"; rest="${rest#*:}"
        count="${rest%%:*}"; size="${rest##*:}"
        img="$WORK/btrfs-$name.img"
        rm -f "$img"
        truncate -s "$size" "$img"
        # shellcheck disable=SC2086
        mkfs.btrfs $args -f "$img" >/dev/null
        mount -o loop "$img" "$MNT"
        mkdir -p "$MNT/many"
        note "filling btrfs-$name with $count files"
        # xargs -P keeps this to seconds rather than minutes.
        seq 1 "$count" | xargs -P4 -I{} sh -c "echo {} > $MNT/many/f{}.txt"
        sync
        umount "$MNT"

        # The reason this fixture exists, checked rather than assumed: a
        # level-0 fs tree means the descent loop is still untested.
        level=$(fs_tree_level "$img")
        [ "${level:-0}" -ge 1 ] || {
            echo "guest-build-images: btrfs-$name's fs tree is level ${level:-?}" >&2
            echo "  $count files did not grow it past a single leaf, so it tests nothing new." >&2
            exit 1
        }
        dump_super "$img"
        publish "$img" "${img%.img}.superdump"
        note "built btrfs-$name ($count files, fs tree level $level)"
    done
}

# ---------------------------------------------------------------------
# rich — written through a COMPRESSING mount and holding a deliberately
# varied tree: a highly compressible file (so zstd actually engages), an
# incompressible one (so the same image still has a plain extent), a
# small file stored inline in its item, a sparse file that is nearly all
# holes, a symlink, and a nested directory.
#
# It is what proves the driver reads a compressed extent while still
# reading everything else on the same filesystem. Without the
# incompressible file, a driver that simply refused every read would
# pass the compression test just as well.
# ---------------------------------------------------------------------
build_rich() {
    local img="$WORK/btrfs-rich.img"
    rm -f "$img"
    truncate -s 600M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop,compress=zstd "$img" "$MNT"
    python3 -c "print('the quick brown fox jumps over the lazy dog '*20000)" > "$MNT/compressed.txt"
    dd if=/dev/urandom of="$MNT/plain.bin" bs=1M count=2 status=none
    echo 'small inline' > "$MNT/inline.txt"
    truncate -s 8M "$MNT/sparse.bin"
    ln -s inline.txt "$MNT/link-short"
    mkdir -p "$MNT/sub/nested" && echo nested > "$MNT/sub/nested/file.txt"
    sync
    umount "$MNT"
    dump_super "$img"
    publish "$img" "${img%.img}.superdump"
    note "built btrfs-rich (compressing mount)"
}

# ---------------------------------------------------------------------
# compression — one filesystem per algorithm, each with a manifest the
# kernel generated.
#
# The manifest is the whole point: it records what Linux says each file
# contains, so the driver's decoders are checked against the encoder
# that produced the bytes rather than against themselves.
#
#   big.txt     compressible and many sectors long, the only way the LZO
#               segment framing shows up at all
#   small.txt   compressible but under one sector: the single-segment case
#   plain.bin   incompressible, so it stays an ordinary extent and the
#               plain path is checked beside the compressed one
#   inline.txt  small enough to live inline in its own item
#
# `compress=`, not `compress-force=`: the incompressible file must stay
# an ordinary extent, so that a driver which simply refused (or mangled)
# every read cannot pass by reading nothing.
# ---------------------------------------------------------------------
build_compression() {
    local algo img manifest record
    for algo in $COMPRESSION_ALGOS; do
        img="$WORK/btrfs-comp-$algo.img"
        manifest="$WORK/btrfs-comp-$algo.manifest"
        record="$WORK/btrfs-comp-$algo.compression"
        rm -f "$img" "$manifest" "$record"
        truncate -s 600M "$img"
        mkfs.btrfs -f "$img" >/dev/null

        mount -o "loop,compress=$algo" "$img" "$MNT"
        python3 -c "print('the quick brown fox jumps over the lazy dog '*40000)" > "$MNT/big.txt"
        python3 -c "print('ab'*200)" > "$MNT/small.txt"
        dd if=/dev/urandom of="$MNT/plain.bin" bs=1M count=2 status=none
        echo 'inline and compressible aaaaaaaaaaaaaaaaaaaaaaaa' > "$MNT/inline.txt"
        sync
        umount "$MNT"

        # What the kernel says each file holds, read back through its own
        # driver on a read-only mount so the image is not disturbed.
        mount -o loop,ro "$img" "$MNT"
        ( cd "$MNT"
          find . -mindepth 1 -type f | sort | while read -r p; do
              printf '%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" \
                  "$(sha256sum "$p" | cut -d' ' -f1)"
          done
        ) > "$manifest"
        umount "$MNT"

        # Which compression types actually ended up on disk. If this does
        # not name the algorithm, the mount option was ignored and the
        # fixture is not testing what it claims to, so the build fails
        # here rather than in a test that reads it as a pass.
        btrfs inspect-internal dump-tree -t 5 "$img" 2>/dev/null |
            grep -o 'extent compression [0-9]* ([a-z]*)' | sort -u > "$record"
        grep -q "($algo)" "$record" || {
            echo "guest-build-images: btrfs-comp-$algo has no $algo-compressed extent on disk: $(tr '\n' ' ' < "$record")" >&2
            exit 1
        }
        dump_super "$img"
        publish "$img" "${img%.img}.superdump" "$manifest" "$record"
        note "built btrfs-comp-$algo"
    done
}

# ---------------------------------------------------------------------
# encoded — a volume whose compressed extents were stored through the
# kernel's encoded-write path rather than compressed by the kernel (#556).
#
# The files are written on a compressing mount: zstd from the mount
# option, zlib and lzo per file from the compression property, and one
# incompressible file that stays plain. A read-only snapshot of them is
# sent with `btrfs send --compressed-data`, which carries the extents
# still compressed, and received onto a second volume mounted WITHOUT a
# compression option. Every compressed extent on that volume was
# therefore handed to the kernel already compressed, through the
# encoded-write ioctl receive uses.
#
# If receive fell back to decompressing (an old kernel, a refused ioctl),
# the received volume would hold only plain extents and the dump-tree
# check below fails the build, rather than a test passing over a fixture
# that does not hold what it claims to.
#
# In its own directory, so the suites that walk every image in
# test-disks/ do not take it up; its own suite is the one that reads it.
# ---------------------------------------------------------------------
build_encoded() {
    local src="$WORK/btrfs-encoded-source.img" img="$WORK/btrfs-encoded.img"
    local manifest="$WORK/btrfs-encoded.manifest" record="$WORK/btrfs-encoded.compression"
    local stream="$WORK/btrfs-encoded.stream" algo
    rm -f "$src" "$img" "$manifest" "$record" "$stream"
    truncate -s 400M "$src"
    mkfs.btrfs -f "$src" >/dev/null
    mount -o loop,compress=zstd "$src" "$MNT"
    btrfs subvolume create "$MNT/vol" >/dev/null
    python3 -c "print('the quick brown fox jumps over the lazy dog '*40000)" > "$MNT/vol/big-zstd.txt"
    for algo in zlib lzo; do
        touch "$MNT/vol/big-$algo.txt"
        btrfs property set "$MNT/vol/big-$algo.txt" compression "$algo"
        python3 -c "print('pack my box with five dozen liquor jugs $algo '*40000)" > "$MNT/vol/big-$algo.txt"
    done
    dd if=/dev/urandom of="$MNT/vol/plain.bin" bs=1M count=2 status=none
    echo 'inline and compressible aaaaaaaaaaaaaaaaaaaaaaaa' > "$MNT/vol/inline.txt"
    sync
    btrfs subvolume snapshot -r "$MNT/vol" "$MNT/sent" >/dev/null
    sync
    btrfs send --proto 2 --compressed-data -f "$stream" "$MNT/sent" >/dev/null
    umount "$MNT"
    rm -f "$src"

    truncate -s 400M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop "$img" "$MNT"
    btrfs receive -f "$stream" "$MNT" >/dev/null
    sync
    umount "$MNT"
    rm -f "$stream"

    # What the kernel says each received file holds, read back through
    # its own driver on a read-only mount so the image is not disturbed.
    mount -o loop,ro "$img" "$MNT"
    ( cd "$MNT"
      find . -mindepth 1 -type f | sort | while read -r p; do
          printf '%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" \
              "$(sha256sum "$p" | cut -d' ' -f1)"
      done
    ) > "$manifest"
    umount "$MNT"

    btrfs inspect-internal dump-tree "$img" 2>/dev/null |
        grep -o 'extent compression [0-9]* ([a-z]*)' | sort -u > "$record"
    for algo in zstd zlib lzo none; do
        grep -q "($algo)" "$record" || {
            echo "guest-build-images: btrfs-encoded has no ($algo) extent after the receive: $(tr '\n' ' ' < "$record")" >&2
            exit 1
        }
    done
    mkdir -p "$OUT/encoded"
    local f
    for f in "$img" "$manifest" "$record"; do
        cp --sparse=always "$f" "$OUT/encoded/$(basename "$f").partial"
        mv -f "$OUT/encoded/$(basename "$f").partial" "$OUT/encoded/$(basename "$f")"
        rm -f "$f"
    done
    note "built encoded/btrfs-encoded (received through encoded writes)"
}

# ---------------------------------------------------------------------
# subvol — a filesystem with subvolumes and snapshots, and a manifest of
# what is in it.
#
# Every geometry fixture has exactly one subvolume: the default fs tree,
# objectid 5. That is not the shape a real btrfs filesystem has —
# subvolumes are how people use it, and a snapshot is how they back it up.
#
#   top        the default subvolume, with files of its own
#   sub        a subvolume beside it
#   sub/inner  a subvolume nested inside that one
#   snap       a snapshot of `sub`, taken before `sub` is written to again
#   rosnap     a read-only snapshot of `sub`
#   top/nested a subvolume inside the ordinary directory `top`, made last
#              so the ids of the others do not move
#
# `top/nested` is there because its path is not the chain of subvolume
# names: `btrfs subvolume list` prints `top/nested`, the directory it sits
# in included, where names alone give `nested` (#362).
#
# `sub` gains a file AFTER `snap` is taken, so the two are genuinely
# divergent rather than identical — a driver that resolved a snapshot to
# its parent's current tree would read the extra file and pass anything
# that only counted names.
#
# `btrfs subvolume list` is the reference answer, recorded beside the
# image while the filesystem is still mounted, so the driver's own
# enumeration is compared against what btrfs-progs reports rather than
# against what the driver believes.
# ---------------------------------------------------------------------
build_subvol() {
    local img="$WORK/btrfs-subvol.img" manifest="$WORK/btrfs-subvol.manifest" s
    rm -f "$img" "$manifest"
    truncate -s 512M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop "$img" "$MNT"

    mkdir -p "$MNT/top"
    echo "in the default subvolume" > "$MNT/top/a.txt"
    btrfs subvolume create "$MNT/sub" >/dev/null
    echo "in sub" > "$MNT/sub/b.txt"
    btrfs subvolume create "$MNT/sub/inner" >/dev/null
    echo "in sub/inner" > "$MNT/sub/inner/c.txt"
    sync
    btrfs subvolume snapshot "$MNT/sub" "$MNT/snap" >/dev/null
    btrfs subvolume snapshot -r "$MNT/sub" "$MNT/rosnap" >/dev/null
    sync
    # Now make `sub` differ from its snapshots.
    echo "added after the snapshot" > "$MNT/sub/after.txt"
    btrfs subvolume create "$MNT/top/nested" >/dev/null
    echo "in top/nested" > "$MNT/top/nested/d.txt"
    sync

    {
        echo "# btrfs subvolume list -pcgu, taken from the mounted filesystem."
        echo "# Columns as btrfs-progs printed them; this is the reference the"
        echo "# driver's own enumeration is compared against."
        btrfs subvolume list -pcgu "$MNT"
        echo "# --- what each subvolume holds, one line per path"
        for s in top sub sub/inner snap rosnap top/nested; do
            [ -e "$MNT/$s" ] || continue
            printf 'contains %s:' "$s"
            find "$MNT/$s" -maxdepth 1 -type f -printf ' %f' 2>/dev/null || true
            echo
        done
        # What the kernel reads at paths that climb with `..`, including
        # out of a subvolume into its parent and out of the empty
        # directory a snapshot shows for a nested subvolume (#271). None
        # climbs above the mount's top, where the kernel would leave the
        # filesystem altogether.
        echo "# --- what the kernel reads at a path with .. in it"
        for s in sub/inner/../b.txt sub/inner/../../top/a.txt \
                 top/../sub/inner/c.txt snap/inner/../b.txt \
                 sub/./inner/.././inner/c.txt; do
            printf 'resolves %s\t%s\n' "$s" "$(cat "$MNT/$s")"
        done
    } > "$manifest"

    umount "$MNT"
    dump_super "$img"
    publish "$img" "${img%.img}.superdump" "$manifest"
    note "built btrfs-subvol"
}

# ---------------------------------------------------------------------
# xattr — a filesystem carrying extended attributes, and a manifest of
# what the kernel says is on it.
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
#                 name, so this must come back spelled in full.
#
# The two colliding names hash to 0x5bc5594f under crc32c(~1, name).
# Regenerating the pair means searching the same hash for another
# collision — the numbers are not arbitrary and cannot be tidied up.
# ---------------------------------------------------------------------
build_xattr() {
    local img="$WORK/btrfs-xattr.img" manifest="$WORK/btrfs-xattr.manifest"
    local collide_a="user.tag1371838" collide_b="user.tag2000402"
    rm -f "$img" "$manifest"
    truncate -s 512M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop "$img" "$MNT"

    echo "plain" > "$MNT/plain.txt"
    setfattr -n user.colour -v "blue" "$MNT/plain.txt"
    # -v "" is a zero-length value, not an unset attribute.
    setfattr -n user.empty -v "" "$MNT/plain.txt"
    # 0x… is setfattr's hex form: NUL, high bytes, a newline — the things
    # a value must survive being.
    setfattr -n user.binary -v 0x000102ff7f0a00 "$MNT/plain.txt"
    setfattr -n user.long -v "$(printf 'x%.0s' $(seq 1 2000))" "$MNT/plain.txt"

    echo "collide" > "$MNT/collide.txt"
    setfattr -n "$collide_a" -v "first of the pair" "$MNT/collide.txt"
    setfattr -n "$collide_b" -v "second of the pair" "$MNT/collide.txt"

    echo "bare" > "$MNT/bare.txt"

    mkdir -p "$MNT/dir"
    setfattr -n user.on-a-directory -v "yes" "$MNT/dir"
    echo "inner" > "$MNT/dir/inner.txt"
    setfattr -n trusted.root-only -v "only root may set this" "$MNT/dir/inner.txt"
    sync

    # `-e hex` so a binary value survives being written down; `-d -m -`
    # so every namespace is dumped, not just `user`; paths relative to
    # the mount point so the manifest carries no temporary name.
    {
        echo "# getfattr -R -d -m - -e hex, from the mounted filesystem."
        echo "# The reference the driver's own listing is compared against."
        echo "# An attribute with a zero-length value prints as a bare name,"
        echo "# with no '=' — that is getfattr's spelling, not a truncation."
        (cd "$MNT" && getfattr -R -d -m - -e hex .)
    } > "$manifest"

    umount "$MNT"
    dump_super "$img"
    publish "$img" "${img%.img}.superdump" "$manifest"
    note "built btrfs-xattr"
}

# ---------------------------------------------------------------------
# acl — POSIX ACLs, set by setfacl and inherited by the kernel, on a
# 4 KiB-node and a 16 KiB-node filesystem, and the kernel's own account
# of every one of them.
#
# An ACL is an extended attribute (`system.posix_acl_access`, or
# `system.posix_acl_default` on a directory) whose value has a format of
# its own: a 4-byte header carrying version 2, then one 8-byte entry per
# ACL entry. The value is the kernel's to write, so everything here is
# set through setfacl or created under a default ACL and left for the
# kernel to fill in.
#
#   minimal.txt       an ACL setfacl was given that the mode bits already
#                     say. The kernel stores NO attribute for it, and a
#                     driver must not invent one.
#   one-user.txt      the smallest ACL that is stored: one named user,
#                     which brings a mask with it — five entries.
#   mixed.txt         named users AND named groups, so the tags sort.
#   sweep/            one file per entry count, from five up to the
#                     largest the node size admits (see below).
#   over.txt, over.d  one entry past that largest: the kernel refuses it,
#                     so these carry no ACL at all.
#   inherit/          a directory with access and default ACLs; the file,
#                     the subdirectory and the file inside that were
#                     CREATED under it, so their ACLs are the kernel's
#                     inheritance, not anything setfacl was told.
#   maxdir/           a directory whose access ACL AND default ACL are
#                     both the largest the node size admits — two items
#                     that cannot share a leaf on a 4 KiB node — and a
#                     file and a directory created under it, which
#                     inherit that largest default.
#
# THE BOUNDARY. Btrfs keeps every attribute inline, in one item of one
# leaf; there is no out-of-line value to spill into. So the largest ACL
# is set by the node size: the kernel refuses a value whose name and
# value would not fit an item in an otherwise empty leaf, with ENOSPC.
# That largest count is FOUND here, by bisecting on what setfacl is
# allowed to set, rather than computed — the arithmetic is the test's
# to check against it — and recorded in the manifest with the refusal
# the next count up met.
#
# The manifest is `getfattr -R -d -m - -e hex`, as the xattr fixture's,
# preceded by `#`-lines the xattr parser skips:
#   # acl-nodesize: <bytes>
#   # acl-max: <access|default> <largest accepted> refused <next>: <setfacl's error>
#   # acl-count: <path> access <n> default <n>      (getfacl's entry counts)
#   # acl-value: <path> <name> 0x<hex>              (getfattr -n <name>)
# ---------------------------------------------------------------------

# acl_spec <total> [default] — an ACL of exactly <total> entries for
# `setfacl -M`: the four a named entry requires (owner, owning group,
# mask, other) and <total - 4> named users with distinct ids.
acl_spec() {
    local total="$1" prefix="${2:+default:}" i
    printf '%suser::rw-\n%sgroup::r--\n%smask::rw-\n%sother::r--\n' \
        "$prefix" "$prefix" "$prefix" "$prefix"
    for ((i = 0; i < total - 4; i++)); do
        printf '%suser:%d:r--\n' "$prefix" $((10000 + i))
    done
}

# acl_set <path> <total> [default] — set that ACL; setfacl's error, if
# any, is left in $WORK/acl-err. `-n`: the spec's mask stands as given,
# so the entry count is exactly <total>.
acl_set() {
    acl_spec "$2" "${3:-}" > "$WORK/acl-spec"
    setfacl -n -M "$WORK/acl-spec" "$1" 2> "$WORK/acl-err"
}

# acl_max <path> [default] — the largest entry count the kernel accepts
# on <path>, by bisection between a count it must accept and one it must
# refuse. Either end behaving otherwise fails the build: the search
# would then be measuring something other than the boundary.
acl_max() {
    local path="$1" kind="${2:-}" lo=5 hi=8200 mid
    setfacl -b "$path"
    if ! acl_set "$path" "$lo" "$kind"; then
        echo "guest-build-images.sh: a $lo-entry ${kind:-access} ACL was refused: $(cat "$WORK/acl-err")" >&2
        return 1
    fi
    setfacl -b "$path"
    if acl_set "$path" "$hi" "$kind"; then
        echo "guest-build-images.sh: a $hi-entry ${kind:-access} ACL was accepted — larger than any value an attribute can hold" >&2
        return 1
    fi
    while (( hi - lo > 1 )); do
        mid=$(( (lo + hi) / 2 ))
        setfacl -b "$path"
        if acl_set "$path" "$mid" "$kind"; then lo=$mid; else hi=$mid; fi
    done
    setfacl -b "$path"
    echo "$lo"
}

# acl_refused <path> <total> [default] — the ACL must be refused, and
# refused for want of room. Prints setfacl's error.
acl_refused() {
    if acl_set "$@"; then
        echo "guest-build-images.sh: a $2-entry ${3:-access} ACL on $1 was accepted, one past the largest" >&2
        return 1
    fi
    if ! grep -q 'No space left on device' "$WORK/acl-err"; then
        echo "guest-build-images.sh: the $2-entry ${3:-access} ACL was refused for another reason: $(cat "$WORK/acl-err")" >&2
        return 1
    fi
    # Relative to the mount point, so the manifest names no temporary path.
    tr '\n' ' ' < "$WORK/acl-err" | sed -e "s|$MNT/|/|g" -e 's/ *$//'
}

build_acl() {
    local geometry name nodesize img manifest max_access max_default
    local refused_access refused_default n p a d attr v
    for geometry in "node4k:4096" "node16k:16384"; do
        name="${geometry%%:*}"
        nodesize="${geometry#*:}"
        img="$WORK/btrfs-acl-$name.img"
        manifest="$WORK/btrfs-acl-$name.manifest"
        rm -f "$img" "$manifest"
        truncate -s 256M "$img"
        mkfs.btrfs -f -n "$nodesize" "$img" >/dev/null
        # `acl` is btrfs's default, named anyway so a changed default
        # cannot quietly build a fixture with no ACLs in it.
        mount -o loop,acl "$img" "$MNT"

        # The boundary, measured on scratch paths removed afterwards.
        touch "$MNT/scratch"
        max_access="$(acl_max "$MNT/scratch")"
        rm -f "$MNT/scratch"
        mkdir "$MNT/scratch.d"
        max_default="$(acl_max "$MNT/scratch.d" default)"
        rmdir "$MNT/scratch.d"

        echo minimal > "$MNT/minimal.txt"
        setfacl -m u::rw-,g::r--,o::r-- "$MNT/minimal.txt"
        echo one > "$MNT/one-user.txt"
        setfacl -m u:1000:rw- "$MNT/one-user.txt"
        echo mixed > "$MNT/mixed.txt"
        setfacl -m u:1001:r--,u:1002:rw-,g:2001:r--,g:2002:rwx "$MNT/mixed.txt"

        mkdir "$MNT/sweep"
        n=5
        while (( n < max_access - 1 )); do
            echo "$n" > "$MNT/sweep/entries-$n.txt"
            acl_set "$MNT/sweep/entries-$n.txt" "$n"
            n=$(( n * 2 ))
        done
        for n in $(( max_access - 1 )) "$max_access"; do
            echo "$n" > "$MNT/sweep/entries-$n.txt"
            acl_set "$MNT/sweep/entries-$n.txt" "$n"
        done

        echo over > "$MNT/over.txt"
        refused_access="$(acl_refused "$MNT/over.txt" $(( max_access + 1 )))"
        mkdir "$MNT/over.d"
        refused_default="$(acl_refused "$MNT/over.d" $(( max_default + 1 )) default)"

        mkdir "$MNT/inherit"
        setfacl -m u:1000:rwx,g:2000:r-x,d:u:1000:rwx,d:g:2000:r-x,d:o::--- "$MNT/inherit"
        echo file > "$MNT/inherit/file.txt"
        mkdir "$MNT/inherit/sub"
        echo nested > "$MNT/inherit/sub/nested.txt"

        mkdir "$MNT/maxdir"
        acl_set "$MNT/maxdir" "$max_access"
        acl_set "$MNT/maxdir" "$max_default" default
        echo child > "$MNT/maxdir/child.txt"
        mkdir "$MNT/maxdir/child.d"
        sync

        {
            echo "# POSIX ACLs on a ${nodesize}-byte-node btrfs, as the kernel reports them."
            echo "# acl-nodesize: $nodesize"
            echo "# acl-max: access $max_access refused $(( max_access + 1 )): $refused_access"
            echo "# acl-max: default $max_default refused $(( max_default + 1 )): $refused_default"
            ( cd "$MNT" && find . -mindepth 1 | sort | while read -r p; do
                a="$(getfacl -cnE "$p" | grep -cv -e '^default:' -e '^$' || true)"
                d="$(getfacl -cnE "$p" | grep -c '^default:' || true)"
                echo "# acl-count: ${p#.} access $a default $d"
                for attr in system.posix_acl_access system.posix_acl_default; do
                    v="$(getfattr -e hex -n "$attr" "$p" 2>/dev/null | sed -n "s/^$attr=//p" || true)"
                    [ -z "$v" ] || echo "# acl-value: ${p#.} $attr $v"
                done
              done )
            echo "# getfattr -R -d -m - -e hex, from the mounted filesystem."
            (cd "$MNT" && getfattr -R -d -m - -e hex .)
        } > "$manifest"

        # The reference must actually carry ACLs, from BOTH readings of
        # them, or every comparison against it compares nothing.
        for attr in system.posix_acl_access system.posix_acl_default; do
            grep -q "^# acl-value: .* $attr 0x" "$manifest" \
                || { echo "guest-build-images.sh: getfattr -n reported no $attr on btrfs-acl-$name" >&2; exit 1; }
            grep -q "^$attr=0x" "$manifest" \
                || { echo "guest-build-images.sh: getfattr -d -m - listed no $attr on btrfs-acl-$name" >&2; exit 1; }
        done

        umount "$MNT"
        dump_super "$img"
        publish "$img" "${img%.img}.superdump" "$manifest"
        note "built btrfs-acl-$name (largest ACL: $max_access access, $max_default default entries)"
    done
}

# ---------------------------------------------------------------------
# nodatacow — one filesystem holding a checksummed file and a file with
# no checksums, so a driver's data-checksum path can be judged on both.
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
# And a second image: the same file AFTER A SNAPSHOT (#63). A snapshot
# does NOT raise a data extent's reference count when the subvolume's
# tree is taller than one leaf — `btrfs_copy_root` adds a reference to
# each block the root points at, and those are tree blocks, not data. So
# `nc/inplace.bin`'s extent still reads `refs 1` while the snapshot
# reads the same bytes. The kernel tells the two apart with the fs
# tree's `last_snapshot`. The 3000 empty files in `many/` are only there
# so the fs tree is a node before the snapshot is taken, and the build
# checks that it is.
#
# It goes in its own directory: the suites that walk every image in
# test-disks/ assume what an ordinary volume holds, and a snapshot
# breaks that on purpose — the blocks under the shared root carry two
# inline references, a 42-byte METADATA_ITEM where every other image has
# 33.
# ---------------------------------------------------------------------
build_nodatacow() {
    local img="$WORK/btrfs-nodatacow.img" level
    rm -f "$img"
    truncate -s 512M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop "$img" "$MNT"
    mkdir "$MNT/nc"
    chattr +C "$MNT/nc"
    dd if=/dev/urandom of="$MNT/nc/inplace.bin" bs=4096 count=64 status=none
    dd if=/dev/urandom of="$MNT/cow.bin" bs=4096 count=64 status=none
    # The flags as the kernel recorded them, checked rather than assumed:
    # a build where chattr silently did nothing would produce two
    # ordinary files and an oracle that compares nothing.
    lsattr "$MNT/nc/inplace.bin" "$MNT/cow.bin"
    lsattr -d "$MNT/nc" | grep -q 'C' || {
        echo "guest-build-images: the nc/ directory is not NODATACOW — chattr +C did not take" >&2
        umount "$MNT"
        exit 1
    }
    sync
    umount "$MNT"
    dump_super "$img"
    publish "$img" "${img%.img}.superdump"
    note "built btrfs-nodatacow"

    mkdir -p "$OUT/snapshot"
    img="$WORK/btrfs-nodatacow-snapshot.img"
    rm -f "$img"
    truncate -s 512M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop "$img" "$MNT"
    mkdir "$MNT/nc" "$MNT/many"
    chattr +C "$MNT/nc"
    dd if=/dev/urandom of="$MNT/nc/inplace.bin" bs=4096 count=64 status=none
    (cd "$MNT/many" && seq -f 'f%05g' 1 3000 | xargs touch)
    sync
    btrfs subvolume snapshot -r "$MNT" "$MNT/snap" >/dev/null
    lsattr "$MNT/nc/inplace.bin"
    sync
    umount "$MNT"
    level=$(fs_tree_level "$img")
    [ "${level:-0}" -ge 1 ] || {
        echo "guest-build-images: the fs tree is level ${level:-?}, so the snapshot raised every data reference" >&2
        exit 1
    }
    cp --sparse=always "$img" "$OUT/snapshot/btrfs-nodatacow-snapshot.img.partial"
    mv -f "$OUT/snapshot/btrfs-nodatacow-snapshot.img.partial" \
        "$OUT/snapshot/btrfs-nodatacow-snapshot.img"
    rm -f "$img"
    note "built btrfs-nodatacow-snapshot (fs tree level $level)"
}

# ---------------------------------------------------------------------
# commit — the superblock before and after each of six commits.
#
# The superblock is the commit point. Everything a transaction writes is
# invisible until the superblock names the new root, and a writer that
# gets one field of it wrong produces a filesystem the kernel either
# refuses or, worse, mounts against the wrong tree.
#
# SIX COMMITS RATHER THAN ONE: the backup slot index is
# `(generation - 1) mod 4`, so a single commit exercises one slot out of
# four and a writer that hardcoded a slot would pass. Six wraps the ring
# and then some.
#
# BOTH GEOMETRIES COEXIST, under their own suffix. They used to be built
# one after the other under the SAME names, with the oracle run in
# between — which works only in a workflow that interleaves builds and
# tests, and is exactly what a one-task `chore fixtures` cannot do.
# Suffixing them means the suite reads both in one run.
# ---------------------------------------------------------------------
build_commit() {
    local variant suffix args img n gen root commits=6
    for variant in "${GEOMETRY_VARIANTS[@]}"; do
        suffix="${variant%%:*}"
        args="${variant#*:}"
        img="$WORK/btrfs-commit${suffix}.img"
        rm -f "$img" "$WORK"/btrfs-commit"${suffix}"-*.super
        truncate -s 512M "$img"
        # shellcheck disable=SC2086
        mkfs.btrfs $args -f "$img" >/dev/null

        # The superblock as mkfs left it: the "before" of the first
        # commit. At 64 KiB, not at the start of the device — offset 0 is
        # empty, and dumping it gives 4096 zero bytes that read as a
        # generation of 0 and a root of 0, which looks like a filesystem
        # rather than like a mistake.
        dd if="$img" of="$WORK/btrfs-commit${suffix}-0.super" bs=4096 skip=16 count=1 status=none

        for n in $(seq 1 "$commits"); do
            mount -o loop "$img" "$MNT"
            # One file per commit, so each transaction has something to
            # write and the fs tree genuinely changes.
            echo "commit $n" > "$MNT/file-$n.txt"
            sync
            # Unmounted rather than only synced: a mounted filesystem's
            # superblock on disk lags what is in memory, and the point
            # here is what a committed superblock looks like.
            umount "$MNT"
            dd if="$img" of="$WORK/btrfs-commit${suffix}-$n.super" bs=4096 skip=16 count=1 status=none
        done

        dump_super "$img"
        publish "$img" "${img%.img}.superdump" "$WORK"/btrfs-commit"${suffix}"-*.super
        note "built btrfs-commit${suffix}: $((commits + 1)) superblocks, $commits commits apart ($args)"
        for n in $(seq 0 "$commits"); do
            local f="$OUT/btrfs-commit${suffix}-$n.super"
            # generation is at 0x048, root at 0x050, little-endian.
            gen=$(od -An -tu8 -j 72 -N 8 "$f" | tr -d ' ')
            root=$(od -An -tu8 -j 80 -N 8 "$f" | tr -d ' ')
            printf '[guest]   %s  generation %-4s root %s\n' "$(basename "$f")" "$gen" "$root"
        done
    done
}

# ---------------------------------------------------------------------
# cow — one filesystem, before and after one change.
#
# The remaining piece of the write path is the recursion: recording an
# allocation modifies the extent tree, which itself lives in allocated
# blocks. Reasoning about how the kernel breaks that cycle is how a
# writer ends up implementing something plausible and wrong. So it is
# measured instead: a WHOLE IMAGE before and after a single minimal
# metadata change, so every block the kernel rewrote can be identified
# and every item it changed can be diffed.
#
#   btrfs-cow-before.img    straight after mkfs, mounted and unmounted
#                           once so the first-mount feature write is
#                           already done and does not pollute the diff
#   btrfs-cow-control.img   mounted and unmounted again, CHANGING NOTHING
#   btrfs-cow-after.img     one `touch` and one `sync` later
#
# The control is what makes the measurement mean anything. Mounting a
# filesystem read-write commits by itself, so a before/after pair around
# a `touch` contains the touch AND whatever a bare mount cycle does.
# Without a pair that did nothing, every one of those writes would be
# attributed to creating the file.
# ---------------------------------------------------------------------
build_cow() {
    local variant suffix args before control after f gen root used
    for variant in "${GEOMETRY_VARIANTS[@]}"; do
        suffix="${variant%%:*}"
        args="${variant#*:}"
        before="$WORK/btrfs-cow-before${suffix}.img"
        control="$WORK/btrfs-cow-control${suffix}.img"
        after="$WORK/btrfs-cow-after${suffix}.img"
        rm -f "$before" "$control" "$after"
        truncate -s 512M "$before"
        # shellcheck disable=SC2086
        mkfs.btrfs $args -f "$before" >/dev/null

        # Settle the first-mount writes so they are not part of the diff.
        mount -o loop "$before" "$MNT"
        umount "$MNT"

        cp --sparse=always "$before" "$control"
        cp --sparse=always "$before" "$after"

        # The control: the same mount cycle, changing nothing.
        mount -o loop "$control" "$MNT"
        sync
        umount "$MNT"

        # The change under study: one empty file, one sync. The smallest
        # metadata transaction there is.
        mount -o loop "$after" "$MNT"
        touch "$MNT/one"
        sync
        umount "$MNT"

        for f in "$before" "$control" "$after"; do
            gen=$(od -An -tu8 -j $((65536 + 72)) -N 8 "$f" | tr -d ' ')
            root=$(od -An -tu8 -j $((65536 + 80)) -N 8 "$f" | tr -d ' ')
            used=$(od -An -tu8 -j $((65536 + 120)) -N 8 "$f" | tr -d ' ')
            printf '[guest]   %-32s generation %-4s root %-12s bytes_used %s\n' \
                "$(basename "$f")" "$gen" "$root" "$used"
        done
        publish "$before" "$control" "$after"
        note "built btrfs-cow${suffix}: before, control (no change) and after (one touch) ($args)"
    done
}

# ---------------------------------------------------------------------
# split — one leaf split, caught either side.
#
# `src/leaf_edit.rs` refuses an item that will not fit rather than
# splitting the leaf, because where the kernel puts the boundary is a
# policy. Measuring the leaves of an existing filesystem showed the
# median is 91-98% FULL, so it is plainly not "half" — but a
# distribution says what the results look like, not what the rule is.
#
# This catches the event itself. Files are added one at a time, each
# committed, and after each the fs tree's leaves are counted from the
# reference tool's own dump. When the count goes up, the previous image
# is the "before" and the current one is the "after".
#
# The `-vary` pair makes the items WILDLY different sizes, by
# alternating a near-maximum filename with a one-character one. That is
# the experiment that separates the two candidate rules: a split at half
# the ITEM COUNT does not care about sizes, and a split at half the
# BYTES lands somewhere else entirely once the items are uneven. With
# every item the same size the two agree and the measurement says
# nothing.
# ---------------------------------------------------------------------
build_split() {
    local vary suffix work prev before after last now i name split_at max=400
    for vary in 0 1; do
        suffix=""
        [ "$vary" = 1 ] && suffix="-vary"
        work="$WORK/btrfs-split-work.img"
        prev="$WORK/btrfs-split-prev.img"
        before="$WORK/btrfs-split${suffix}-before.img"
        after="$WORK/btrfs-split${suffix}-after.img"
        rm -f "$work" "$prev" "$before" "$after"

        truncate -s 512M "$work"
        # The smallest nodesize btrfs accepts, so a leaf fills quickly.
        mkfs.btrfs -n 4096 -f "$work" >/dev/null

        # Settle the first-mount writes before anything is measured.
        mount -o loop "$work" "$MNT"
        umount "$MNT"

        last=$(btrfs inspect-internal dump-tree -t 5 "$work" 2>/dev/null | grep -c '^leaf ' || true)
        note "split${suffix}: start: $last leaf/leaves in the fs tree"

        split_at=""
        for i in $(seq 1 "$max"); do
            cp --sparse=always "$work" "$prev"
            mount -o loop "$work" "$MNT"
            if [ "$vary" = 1 ] && [ $((i % 2)) -eq 0 ]; then
                name="$(printf 'x%.0s' $(seq 1 200))-$i"
            else
                name="f$i"
            fi
            echo "$i" > "$MNT/$name"
            sync
            umount "$MNT"

            now=$(btrfs inspect-internal dump-tree -t 5 "$work" 2>/dev/null | grep -c '^leaf ' || true)
            if [ "$now" -gt "$last" ]; then
                split_at="$i"
                cp --sparse=always "$prev" "$before"
                cp --sparse=always "$work" "$after"
                {
                    echo "files_before_split=$((i - 1))"
                    echo "leaves_before=$last"
                    echo "leaves_after=$now"
                    echo "nodesize=4096"
                } > "$WORK/btrfs-split${suffix}.txt"
                note "split${suffix}: at file $i: $last leaves -> $now"
                break
            fi
            last="$now"
        done
        rm -f "$prev" "$work"
        [ -n "$split_at" ] || {
            echo "guest-build-images: no leaf split after $max files in the ${suffix:-default} run" >&2
            exit 1
        }
        publish "$before" "$after" "$WORK/btrfs-split${suffix}.txt"
        note "built btrfs-split${suffix}-before.img and btrfs-split${suffix}-after.img"
    done
}

# ---------------------------------------------------------------------
# pool — a btrfs filesystem spanning two devices.
#
# Everything else in the matrix is one image. A pool is different in
# kind: a chunk stripe names the device it lives on, so reading one disk
# of a two-disk filesystem is not a partial read, it is a read of the
# WRONG BYTES — they parse, and on a mirrored pool they may even
# checksum. What a reader is expected to do with one of them is refuse,
# and tests/pool_oracle.rs holds it to that.
# ---------------------------------------------------------------------
build_pool() {
    local a="$WORK/btrfs-pool-a.img" b="$WORK/btrfs-pool-b.img"
    local manifest="$WORK/btrfs-pool.manifest" la lb i f devs
    rm -f "$a" "$b" "$manifest"
    truncate -s 512M "$a"
    truncate -s 512M "$b"

    # Loop devices, because mkfs.btrfs takes devices rather than files
    # when building a multi-device filesystem.
    la=$(losetup --find --show "$a")
    lb=$(losetup --find --show "$b")
    pool_cleanup() {
        mountpoint -q "$MNT" && umount "$MNT" || true
        losetup -d "$la" 2>/dev/null || true
        losetup -d "$lb" 2>/dev/null || true
    }
    trap 'pool_cleanup; cleanup' EXIT

    mkfs.btrfs -f -d raid1 -m raid1 "$la" "$lb" >/dev/null

    # Content, so a reader has something to be right or wrong about. An
    # empty pool proves only that the superblock parses.
    mount "$la" "$MNT"
    mkdir -p "$MNT/dir"
    for i in 1 2 3; do
        echo "pool file $i" > "$MNT/dir/file-$i.txt"
    done
    # Big enough to need a data extent rather than living inline in its
    # item, so the chunk mapping is actually exercised on a read.
    dd if=/dev/urandom of="$MNT/big.bin" bs=1M count=4 status=none
    sync
    # A file with no data checksums, which is what the copy-on-write
    # write path takes so far, so a pool can be written through it and
    # handed back to the kernel (#298). A remount with `nodatasum` marks
    # every inode created under it NODATASUM for good.
    mount -o remount,nodatasum "$MNT"
    dd if=/dev/urandom of="$MNT/nosum.bin" bs=64K count=2 status=none
    sync

    # Size AND content. A size alone is a weak check on a mirrored pool:
    # reading four megabytes of the WRONG bytes has the right length.
    ( cd "$MNT" && find . -mindepth 1 | sort | while read -r p; do
        if [ -f "$p" ]; then
            printf '%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" \
                "$(sha256sum "$p" | cut -d' ' -f1)"
        else
            printf '%s\tdir\t-\n' "${p#.}"
        fi
      done ) > "$manifest"

    umount "$MNT"
    losetup -d "$la"
    losetup -d "$lb"
    trap cleanup EXIT

    for f in "$a" "$b"; do
        devs=$(od -An -tu8 -j $((65536 + 0x88)) -N 8 "$f" | tr -d ' ')
        printf '[guest]   %s  num_devices %s\n' "$(basename "$f")" "$devs"
    done
    publish "$a" "$b" "$manifest"
    note "built btrfs-pool-a and btrfs-pool-b (two-device RAID1 with content)"

    build_parity_pool raid5 3
    build_parity_pool raid6 4
    # The same with a nodatasum file, which the copy-on-write write path
    # takes, so a parity pool can be written and handed back to the
    # kernel (#299). Separate images: on the pools above every file has
    # checksums, which the damaged-device tests rely on to tell a rebuilt
    # element from a corrupt one.
    build_parity_pool raid5 3 nosum
    build_parity_pool raid6 4 nosum
    # And one with no redundancy at all, which a degraded mount must
    # refuse: losing either member loses bytes nothing else holds (#300).
    build_parity_pool raid0 2
}

# ---------------------------------------------------------------------
# parity pools — RAID5 on three devices and RAID6 on four, data AND
# metadata, with a manifest of what the kernel wrote (#268).
#
# Parity rotates from one full stripe to the next, so a reader that
# placed the data elements by any other rule reads parity as data and
# returns the right length of the wrong bytes. The 8 MiB file crosses
# dozens of full stripes, which is every rotation many times over, and
# the manifest's digests are what make that a check on the bytes.
#
# Members are named btrfs-<profile>-<n>.img for n = 1.., in device-id
# order as mkfs.btrfs assigns them (the order the devices are given).
# ---------------------------------------------------------------------
build_parity_pool() {
    local profile="$1" count="$2" variant="${3:-}" i f loops=() imgs=()
    local name="btrfs-$profile${variant:+-$variant}"
    local manifest="$WORK/$name.manifest"
    for i in $(seq 1 "$count"); do
        f="$WORK/$name-$i.img"
        rm -f "$f"
        truncate -s 256M "$f"
        imgs+=("$f")
        loops+=("$(losetup --find --show "$f")")
    done
    parity_cleanup() {
        mountpoint -q "$MNT" && umount "$MNT" || true
        for f in "${loops[@]}"; do losetup -d "$f" 2>/dev/null || true; done
    }
    trap 'parity_cleanup; cleanup' EXIT

    mkfs.btrfs -f -d "$profile" -m "$profile" "${loops[@]}" >/dev/null 2>&1

    mount "${loops[0]}" "$MNT"
    mkdir -p "$MNT/dir"
    for i in 1 2 3; do
        echo "$profile file $i" > "$MNT/dir/file-$i.txt"
    done
    dd if=/dev/urandom of="$MNT/big.bin" bs=1M count=8 status=none
    # Odd-sized, so a file ends partway through a stripe element.
    dd if=/dev/urandom of="$MNT/odd.bin" bs=1 count=200003 status=none
    sync
    if [ "$variant" = nosum ]; then
        # Five 64 KiB elements' worth, so a write lands in whole full
        # stripes and in part of one. A remount with `nodatasum` marks
        # every inode created under it NODATASUM for good.
        mount -o remount,nodatasum "$MNT"
        dd if=/dev/urandom of="$MNT/nosum.bin" bs=64K count=5 status=none
        sync
    fi
    btrfs filesystem df "$MNT" | grep -qi "data, $profile" || {
        echo "guest-build-images: the $profile pool holds no $profile data chunk" >&2
        exit 1
    }

    ( cd "$MNT" && find . -mindepth 1 | sort | while read -r p; do
        if [ -f "$p" ]; then
            printf '%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" \
                "$(sha256sum "$p" | cut -d' ' -f1)"
        else
            printf '%s\tdir\t-\n' "${p#.}"
        fi
      done ) > "$manifest"

    umount "$MNT"
    # btrfs check reads every tree through the parity profile; a pool it
    # does not find clean is not a reference worth holding a reader to.
    btrfs check --readonly "${loops[0]}" >/dev/null 2>&1 || {
        echo "guest-build-images: btrfs check does not find the $profile pool clean" >&2
        exit 1
    }
    for f in "${loops[@]}"; do losetup -d "$f"; done
    trap cleanup EXIT

    publish "${imgs[@]}" "$manifest"
    note "built $name-1..$count ($count-device ${profile^^} with content${variant:+, $variant})"
}

# ---------------------------------------------------------------------
# dirtylog — a filesystem with a log tree the kernel has not replayed.
#
# `fsync` writes a log tree and points the superblock's `log_root` at it;
# the NEXT mount replays it into the real trees. Until then the committed
# trees hold durable.txt as it was before the fsync, and fsynced.txt not
# at all — so a reader that ignored `log_root` would return exactly the
# bytes an application was told had been durably replaced.
#
# The image is copied WHILE MOUNTED, after the fsyncs and before any
# commit: the mount's commit interval is five minutes, and the unmount
# that follows would replay nothing but commit everything, erasing the
# very state being captured. The loop device writes through the backing
# file's page cache, so the copy sees every write the fsyncs made.
#
# The copy's `log_root` is checked, not assumed: a kernel that chose a
# full commit over a log would leave an ordinary image, and the test
# reading it would be testing nothing.
#
# In its own directory, like the snapshot: the suites that walk every
# image in test-disks/ mount each one, and this one is refused on
# purpose.
# ---------------------------------------------------------------------
build_dirtylog() {
    local img="$WORK/btrfs-dirty-log.img" copy="$WORK/btrfs-dirty-log-copy.img" log_root
    rm -f "$img" "$copy"
    truncate -s 256M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop,commit=300 "$img" "$MNT"
    printf 'committed by a sync\n' > "$MNT/durable.txt"
    sync
    printf 'fsynced, not synced\n' | dd of="$MNT/durable.txt" conv=notrunc,fsync status=none
    printf 'created then fsynced\n' | dd of="$MNT/fsynced.txt" conv=fsync status=none
    cp --sparse=always "$img" "$copy"
    umount "$MNT"
    rm -f "$img"
    btrfs inspect-internal dump-super "$copy" > "$WORK/dirty-log.super"
    log_root=$(awk '$1 == "log_root" { print $2 }' "$WORK/dirty-log.super")
    if [ -z "$log_root" ] || [ "$log_root" = 0 ]; then
        echo "guest-build-images: the dirty-log image has log_root '${log_root}' — the fsync did not leave a log tree" >&2
        exit 1
    fi
    mkdir -p "$OUT/dirtylog"
    cp --sparse=always "$copy" "$OUT/dirtylog/btrfs-dirty-log.img.partial"
    mv -f "$OUT/dirtylog/btrfs-dirty-log.img.partial" "$OUT/dirtylog/btrfs-dirty-log.img"
    rm -f "$copy" "$WORK/dirty-log.super"
    note "built btrfs-dirty-log (log_root $log_root)"
    build_dirtylog_replay
    build_dirtylog_inode
    build_dirtylog_inline
    build_dirtylog_refused mixedlog 'compress=no,max_inline=2048' mixedlog
    build_dirtylog_refused compressedinline 'compress-force=zlib,max_inline=2048' compressedinline
    build_dirtylog_refused externalextent 'compress=no,max_inline=0' externalextent
    build_dirtylog_refused sizechange 'compress=no,max_inline=2048' sizechange
    build_dirtylog_refused xattr 'compress=no,max_inline=2048' xattr
    build_dirtylog_refused subvol 'compress=no,max_inline=2048' subvol
    build_dirtylog_refused newdir 'compress=no,max_inline=2048' newdir
    build_dirtylog_refused symlink 'compress=no,max_inline=2048' symlink
    build_dirtylog_refused special 'compress=no,max_inline=2048' special
    build_dirtylog_refused hardlink 'compress=no,max_inline=2048' hardlink
    build_dirtylog_refused extref 'compress=no,max_inline=2048' extref
    build_dirtylog_refused dirlink 'compress=no,max_inline=2048' dirlink
    build_dirtylog_refused nbytes 'compress=no,max_inline=2048' nbytes
    build_dirtylog_refused extrefexists 'compress=no,max_inline=2048' extrefexists
    build_dirtylog_refused extrefcreated 'compress=no,max_inline=2048' extrefcreated
    build_dirtylog_refused extrefremoved 'compress=no,max_inline=2048' extrefremoved
    build_dirtylog_refused compressedcut 'compress-force=zlib,max_inline=0' compressedcut
    build_dirtylog_refused rmdir 'compress=no,max_inline=2048' rmdir
    build_dirtylog_refused linkremoved 'compress=no,max_inline=2048' linkremoved
    build_dirtylog_refused createdlink 'compress=no,max_inline=2048' createdlink
}

# fsync_path <path> — fsync one file or directory, and nothing else.
#
# Not `sync`, which commits the whole filesystem and would empty the log
# this recipe exists to fill; not `dd conv=fsync`, which cannot open a
# directory.
fsync_path() {
    python3 -c 'import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
os.fsync(fd)
os.close(fd)' "$1"
}

# Metadata-only recovery, separate from the data/name replay fixtures.
# An empty, committed file has no data extents to replay. Its existing
# name and owner stay fixed; only chmod is made durable through fsync.
# See https://btrfs.readthedocs.io/en/latest/btrfs-man5.html (treelog).
build_dirtylog_inode() {
    local img="$WORK/btrfs-dirty-log-inode.img" copy="$WORK/btrfs-dirty-log-inode-copy.img"
    local log_root ino log_tree
    truncate -s 256M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop,commit=300 "$img" "$MNT"
    touch "$MNT/metadata-only"
    chmod 0600 "$MNT/metadata-only"
    chown 12345:23456 "$MNT/metadata-only"
    sync
    ino=$(stat -c '%i' "$MNT/metadata-only")
    chmod 0640 "$MNT/metadata-only"
    fsync_path "$MNT/metadata-only"
    cp --sparse=always "$img" "$copy"
    umount "$MNT"
    log_root=$(btrfs inspect-internal dump-super "$copy" | awk '$1 == "log_root" { print $2 }')
    if [ -z "$log_root" ] || [ "$log_root" = 0 ]; then
        echo "guest-build-images: inode-only fsync left no log; chore fixtures dirtylog must produce one" >&2
        exit 1
    fi
    btrfs inspect-internal dump-tree -t 18446744073709551610 "$copy" \
        > "$OUT/dirtylog/btrfs-dirty-log-inode.logdump"
    # TREE_LOG is the log root tree, not subvolume 5's log tree. Follow
    # its ROOT_ITEM's bytenr explicitly, keeping both native dumps.
    log_tree=$(python3 - "$OUT/dirtylog/btrfs-dirty-log-inode.logdump" <<'PY'
import re, sys
dump = open(sys.argv[1]).read()
keys = re.findall(r'item \d+ key \((\S+) (\S+) (\S+)\)', dump)
assert keys == [('TREE_LOG', 'ROOT_ITEM', '5')], ('unexpected log roots', keys)
roots = re.findall(r'\bbytenr (\d+)\b', dump)
assert len(roots) == 1 and int(roots[0]) > 0, ('missing log tree block', roots)
print(roots[0])
PY
    )
    btrfs inspect-internal dump-tree --follow -b "$log_tree" "$copy" \
        >> "$OUT/dirtylog/btrfs-dirty-log-inode.logdump"
    python3 - "$OUT/dirtylog/btrfs-dirty-log-inode.logdump" "$ino" <<'PY'
import re, sys
dump = open(sys.argv[1]).read()
keys = re.findall(r'item \d+ key \((\S+) (\S+) (\S+)\)', dump)
assert (sys.argv[2], 'INODE_ITEM', '0') in keys, ('missing logged inode', keys)
assert all(kind in ('ROOT_ITEM', 'INODE_ITEM', 'INODE_REF')
           for _, kind, _ in keys), ('not an inode-only log', keys)
PY
    cp --sparse=always "$copy" "$OUT/dirtylog/btrfs-dirty-log-inode.img.partial"
    mv -f "$OUT/dirtylog/btrfs-dirty-log-inode.img.partial" \
        "$OUT/dirtylog/btrfs-dirty-log-inode.img"
    note "built btrfs-dirty-log-inode (log_root $log_root, inode $ino, mode 0600 -> 0640; dump-tree verified inode-only records)"
}

# Same-length overwrite of an existing, committed inline file. The size,
# name and owner stay fixed; only the inline bytes change before fsync:
# 33 bytes over 33, so no byte of the committed text survives.
# The native dump decides the record shape; the producer refuses any log
# that is not inode, inode ref and file extent records.
build_dirtylog_inline() {
    local img="$WORK/btrfs-dirty-log-inline.img" copy="$WORK/btrfs-dirty-log-inline-copy.img"
    local log_root ino log_tree
    truncate -s 256M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop,commit=300,compress=no,max_inline=2048 "$img" "$MNT"
    printf 'committed-inline-bytes-0123456789' > "$MNT/inline-overwrite"
    sync
    ino=$(stat -c '%i' "$MNT/inline-overwrite")
    python3 -c 'import os, sys
fd = os.open(sys.argv[1], os.O_WRONLY)
os.pwrite(fd, b"replayed-inline-bytes-ABCDEFGHIJK", 0)
os.fsync(fd)
os.close(fd)' "$MNT/inline-overwrite"
    cp --sparse=always "$img" "$copy"
    umount "$MNT"
    log_root=$(btrfs inspect-internal dump-super "$copy" | awk '$1 == "log_root" { print $2 }')
    if [ -z "$log_root" ] || [ "$log_root" = 0 ]; then
        echo "guest-build-images: inline overwrite fsync left no log; chore fixtures dirtylog must produce one" >&2
        exit 1
    fi
    # The copy was taken while mounted and is never mounted again, so its
    # digest names the exact crash image this transcript describes.
    {
        printf 'image sha256: %s\n' "$(sha256sum "$copy" | awk '{ print $1 }')"
        btrfs inspect-internal dump-tree -t 18446744073709551610 "$copy"
    } > "$OUT/dirtylog/btrfs-dirty-log-inline.logdump"
    log_tree=$(python3 - "$OUT/dirtylog/btrfs-dirty-log-inline.logdump" <<'PY'
import re, sys
dump = open(sys.argv[1]).read()
keys = re.findall(r'item \d+ key \((\S+) (\S+) (\S+)\)', dump)
assert keys == [('TREE_LOG', 'ROOT_ITEM', '5')], ('unexpected log roots', keys)
roots = re.findall(r'\bbytenr (\d+)\b', dump)
assert len(roots) == 1 and int(roots[0]) > 0, ('missing log tree block', roots)
print(roots[0])
PY
    )
    btrfs inspect-internal dump-tree --follow -b "$log_tree" "$copy" \
        >> "$OUT/dirtylog/btrfs-dirty-log-inline.logdump"
    python3 - "$OUT/dirtylog/btrfs-dirty-log-inline.logdump" "$ino" <<'PY'
import re, sys
dump = open(sys.argv[1]).read()
keys = re.findall(r'item \d+ key \((\S+) (\S+) (\S+)\)', dump)
assert (sys.argv[2], 'INODE_ITEM', '0') in keys, ('missing logged inode', keys)
assert (sys.argv[2], 'EXTENT_DATA', '0') in keys, ('missing logged extent', keys)
assert all(kind in ('ROOT_ITEM', 'INODE_ITEM', 'INODE_REF', 'EXTENT_DATA')
           for _, kind, _ in keys), ('not an inline overwrite log', keys)
assert 'inline' in dump, ('logged extent is not inline', dump)
PY
    test "$(sha256sum "$copy" | awk '{ print $1 }')" = \
        "$(awk '/^image sha256: / { print $3 }' "$OUT/dirtylog/btrfs-dirty-log-inline.logdump")" || {
        echo "guest-build-images: inline crash image changed after its transcript was taken" >&2
        exit 1
    }
    cp --sparse=always "$copy" "$OUT/dirtylog/btrfs-dirty-log-inline.img.partial"
    mv -f "$OUT/dirtylog/btrfs-dirty-log-inline.img.partial" \
        "$OUT/dirtylog/btrfs-dirty-log-inline.img"
    note "built btrfs-dirty-log-inline (log_root $log_root, inode $ino, 33-byte inline overwrite; dump-tree verified record kinds)"
}

# A kernel-made log of a shape replay does not support. Each shape has
# its own image, mount options and mutation; the native transcript is
# bound to the exact image by sha256 and superblock, and the record that
# makes the shape unsupported is asserted from the native dump, not ours.
build_dirtylog_refused() {
    local shape="$1" opts="$2" kind="$3"
    local img="$WORK/btrfs-dirty-log-$shape.img" copy="$WORK/btrfs-dirty-log-$shape-copy.img"
    local out="$OUT/dirtylog/btrfs-dirty-log-$shape" log_root log_tree
    truncate -s 256M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o "loop,commit=300,$opts" "$img" "$MNT"
    case "$kind" in
        mixedlog|sizechange|compressedinline|newdir|symlink|special|hardlink|dirlink)
            printf 'committed-inline-bytes-0123456789' > "$MNT/f" ;;
        externalextent)
            head -c 8192 /dev/zero | tr '\0' 'c' > "$MNT/f" ;;
        extref|nbytes)
            # f with 64 more names in /, each 255 bytes: the first 61 fill
            # its INODE_REF to the largest item a 16 KiB leaf holds, and
            # the rest are committed as INODE_EXTREF items.
            printf 'committed-inline-bytes-0123456789' > "$MNT/f"
            python3 -c 'import os, sys
for i in range(64):
    os.link(sys.argv[1] + "/f", sys.argv[1] + "/" + ("%03d" % i) * 85)' "$MNT" ;;
        extrefexists)
            # f with the same 64 more names in /, three of them committed
            # as INODE_EXTREF items, and an empty directory d beside it.
            mkdir "$MNT/d"
            printf 'committed-inline-bytes-0123456789' > "$MNT/f"
            python3 -c 'import os, sys
for i in range(64):
    os.link(sys.argv[1] + "/f", sys.argv[1] + "/" + ("%03d" % i) * 85)' "$MNT" ;;
        extrefremoved)
            # f with the same 64 more names in /, then every name but the
            # last of them removed, f itself among them: f is committed
            # with one link, no INODE_REF item and one INODE_EXTREF item.
            # k is committed beside it.
            printf 'committed-inline-bytes-0123456789' > "$MNT/f"
            python3 -c 'import os, sys
for i in range(64):
    os.link(sys.argv[1] + "/f", sys.argv[1] + "/" + ("%03d" % i) * 85)
os.unlink(sys.argv[1] + "/f")
for i in range(63):
    os.unlink(sys.argv[1] + "/" + ("%03d" % i) * 85)' "$MNT"
            printf 'kept' > "$MNT/k" ;;
        rmdir)
            # An empty directory e and a file k, committed in /.
            mkdir "$MNT/e"
            printf 'kept' > "$MNT/k" ;;
        linkremoved)
            # A file f with a second name g, both committed in /.
            printf 'kept' > "$MNT/f"
            ln "$MNT/f" "$MNT/g" ;;
        compressedcut)
            # 128 KiB of one byte: compress-force stores it as one
            # compressed extent, the largest the kernel makes.
            head -c 131072 /dev/zero | tr '\0' 'c' > "$MNT/f" ;;
        subvol)
            # A file in the top level and one in a subvolume, committed.
            btrfs subvolume create "$MNT/sv" >/dev/null
            printf 'committed-inline-bytes-0123456789' > "$MNT/f"
            printf 'committed-inline-bytes-0123456789' > "$MNT/sv/g" ;;
        xattr)
            # Two attributes committed: one the log changes, one it removes.
            printf 'committed-inline-bytes-0123456789' > "$MNT/f"
            python3 -c 'import os, sys
os.setxattr(sys.argv[1], "user.keep", b"committed")
os.setxattr(sys.argv[1], "user.gone", b"committed")' "$MNT/f" ;;
    esac
    sync
    python3 -c 'import os, sys
kind, d = sys.argv[1], sys.argv[2]
if kind == "newdir":
    # Directories made after the commit and durable by an fsync of / alone:
    # a, empty, and b, holding an empty file and a directory of its own.
    # The committed f keeps its name outside the range the log speaks for.
    os.mkdir(d + "/a")
    os.mkdir(d + "/b")
    os.mkdir(d + "/b/c")
    open(d + "/b/x", "w").close()
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "symlink":
    # A symbolic link made after the commit and durable by an fsync of /
    # alone: the log holds its inode, its reference and the inline extent
    # that holds its target. The committed f keeps its name.
    os.symlink("target-of-the-link", d + "/l")
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "special":
    # A character device, a block device, a FIFO and a bound UNIX socket
    # made after the commit and durable by an fsync of / alone: the log
    # holds each inode, with its type and device number, and its
    # reference. A minor above 255 keeps the device number encoding
    # honest. The committed f keeps its name.
    import socket, stat
    os.umask(0o022)
    os.mknod(d + "/c", 0o644 | stat.S_IFCHR, os.makedev(240, 300))
    os.mknod(d + "/b", 0o644 | stat.S_IFBLK, os.makedev(241, 5))
    os.mkfifo(d + "/p", 0o644)
    s = socket.socket(socket.AF_UNIX)
    s.bind(d + "/s")
    s.close()
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "hardlink":
    # The committed f is given a second name g, and n is created with
    # data and given a second name m, after the commit. An fsync of f and
    # then one of n make both durable: the log holds f in full, with its
    # link count raised and its reference holding both names, n with its
    # data and both its names, and the entries of / that name g, n and m.
    os.link(d + "/f", d + "/g")
    fd = os.open(d + "/n", os.O_WRONLY | os.O_CREAT, 0o644)
    os.write(fd, b"created-then-linked")
    os.close(fd)
    os.link(d + "/n", d + "/m")
    for name in ("f", "n"):
        fd = os.open(d + "/" + name, os.O_RDONLY); os.fsync(fd); os.close(fd)
    sys.exit(0)
if kind == "extref":
    # The committed f, whose INODE_REF is full, is given a 66th name after
    # the commit, which the kernel stores as one more INODE_EXTREF item.
    # An fsync of / makes it durable: the log holds the entry of / naming
    # it, and f with its link count raised and every reference it has,
    # the new extended one among them. An fsync of f itself would fall
    # back to a full commit and leave no log.
    os.link(d + "/f", d + "/" + "064" * 85)
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "dirlink":
    # The committed f is given a second name g after the commit, and only
    # / is fsynced. The log holds the entry of / naming g, in a range
    # that starts past the committed index of f, and f only as an inode
    # that exists: generation 0, its link count raised, and its reference
    # to / holding both names. The data of f is not logged.
    os.link(d + "/f", d + "/g")
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "extrefexists":
    # The committed f, with INODE_EXTREF items, is given the name g in the
    # committed directory d, and only d is fsynced. The log holds the
    # entry of d naming g, and f only as an inode that exists: generation
    # 0, its link count raised to 66, and its INODE_REF items to / and to
    # d, but none of its INODE_EXTREF items, which stay committed.
    os.link(d + "/f", d + "/d/g")
    dfd = os.open(d + "/d", os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "extrefcreated":
    # After the commit, n is created empty and given 64 more names in /,
    # each 255 bytes: the first 61 fill its INODE_REF to the largest item
    # a 16 KiB leaf holds, and the last three are INODE_EXTREF items. Only
    # / is fsynced. The log holds the entries of / for all 65 names, and
    # n only as an inode that exists, though nothing of it is committed:
    # generation 0, size 0, 65 links, its INODE_REF and its three
    # INODE_EXTREF items. An fsync of n itself would fall back to a full
    # commit and leave no log.
    open(d + "/n", "w").close()
    for i in range(64):
        os.link(d + "/n", d + "/" + ("%03d" % i) * 85)
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "createdlink":
    # After the commit, n is created empty and given a second name m in
    # /, and only / is fsynced. The log holds the entries of / naming n
    # and m, and n only as an inode that exists, though nothing of it is
    # committed: generation 0, size 0, two links, no data, and its
    # reference to / holding both names.
    open(d + "/n", "w").close()
    os.link(d + "/n", d + "/m")
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "extrefremoved":
    # The last name of f, held in its one INODE_EXTREF item, is removed
    # after the commit, and only / is fsynced. The log holds index ranges
    # of / covering the removed index and no entry there, and does not
    # hold f: its replay deletes f with its extended reference and data.
    os.unlink(d + "/" + "063" * 85)
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "rmdir":
    # Remove the committed empty directory e through a replacing rename.
    # On Debian 6.1, rmdir followed by a parent fsync commits the whole
    # transaction and leaves no log. A new directory replaces e, then moves
    # away before only / is fsynced: the committed index of e is absent
    # from the log, and replay must remove its old inode with all its items.
    # Diagnostic seeds for #654 run only if this recipe fails; each seed
    # must also satisfy the existing native attestation.
    seed = os.environ.get("RMDIR_LOG_SEED", "")
    def fsync_path(path):
        fd = os.open(path, os.O_RDONLY); os.fsync(fd); os.close(fd)
    if seed == "parent-chmod":
        os.chmod(d, 0o700)
        fsync_path(d)
    elif seed == "parent-create":
        open(d + "/new", "w").close()
        fsync_path(d)
    elif seed == "child-chmod":
        os.chmod(d + "/e", 0o700)
        fsync_path(d + "/e")
    elif seed == "kept-write":
        fd = os.open(d + "/k", os.O_WRONLY)
        os.write(fd, b"kept"); os.fsync(fd); os.close(fd)
    os.mkdir(d + "/replacement")
    os.rename(d + "/replacement", d + "/e")
    os.rename(d + "/e", d + "/replacement")
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
if kind == "linkremoved":
    # The committed name g of f is removed after the commit, and only / is
    # fsynced. The log holds index ranges of / covering the index of g and
    # no entry there, and does not hold f: its replay removes the name g
    # and its reference, and leaves f with one link.
    os.unlink(d + "/g")
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
    sys.exit(0)
fd = os.open(d + "/f", os.O_WRONLY)
if kind == "sizechange":
    os.pwrite(fd, b"replayed-and-longer-inline-bytes-0123456789", 0)
elif kind == "externalextent":
    os.pwrite(fd, b"r" * 8192, 0)
elif kind == "compressedcut":
    # One sector inside the committed compressed extent: the kernel
    # writes it uncompressed, and the committed extent keeps its head
    # and its tail as windows into the whole compressed extent.
    os.pwrite(fd, b"r" * 4096, 32768)
elif kind == "subvol":
    # Both subvolumes change after the commit, so the log root tree names
    # a log tree for each: f in the top level and g in the subvolume are
    # rewritten in place, and a file is created in the subvolume.
    os.pwrite(fd, b"replayed-inline-bytes-ABCDEFGHIJK", 0)
    g = os.open(d + "/sv/g", os.O_WRONLY)
    os.pwrite(g, b"replayed-inline-bytes-ABCDEFGHIJK", 0)
    os.fsync(g)
    os.close(g)
    n = os.open(d + "/sv/new", os.O_WRONLY | os.O_CREAT, 0o644)
    os.write(n, b"created in the subvolume\n")
    os.fsync(n)
    os.close(n)
elif kind == "xattr":
    # Changed, removed and added after the commit; the data is untouched.
    os.setxattr(d + "/f", "user.keep", b"changed after the commit")
    os.removexattr(d + "/f", "user.gone")
    os.setxattr(d + "/f", "user.new", b"added after the commit")
elif kind == "compressedinline":
    # Same 33-byte length, but one repeated byte: zlib shrinks it, so
    # compress-force stores it as a compressed inline extent. The earlier
    # text payload did not shrink and fell back to an uncompressed one.
    os.pwrite(fd, b"z" * 33, 0)
elif kind == "nbytes":
    # The committed f, with INODE_EXTREF items, rewritten in place with
    # 33 other bytes and made durable by an fsync of f alone. The log
    # holds f in full, and the kernel leaves its nbytes holding stale
    # bytes of a long name rather than a count.
    os.pwrite(fd, b"replayed-inline-bytes-ABCDEFGHIJK", 0)
else:
    os.pwrite(fd, b"replayed-inline-bytes-ABCDEFGHIJK", 0)
if kind == "mixedlog":
    open(d + "/g", "w").close()
    os.rename(d + "/g", d + "/h")
os.fsync(fd)
if kind == "mixedlog":
    dfd = os.open(d, os.O_RDONLY); os.fsync(dfd); os.close(dfd)
os.close(fd)' "$kind" "$MNT"
    cp --sparse=always "$img" "$copy"
    umount "$MNT"
    log_root=$(btrfs inspect-internal dump-super "$copy" | awk '$1 == "log_root" { print $2 }')
    if [ -z "$log_root" ] || [ "$log_root" = 0 ]; then
        echo "guest-build-images: $shape fsync left no log; chore fixtures dirtylog must produce one" >&2
        if [ "$kind" = rmdir ] && [ -z "${RMDIR_LOG_SEED:-}" ]; then
            # Run native experiments in the CI guest when the original
            # recipe fails (#654). Keep this failure: no seed substitutes
            # for the requested fixture, and none bypasses its assertions.
            local seed pid
            for seed in parent-chmod parent-create child-chmod kept-write; do
                echo "[rmdir diagnostic] $seed"
                ( export RMDIR_LOG_SEED="$seed"
                  build_dirtylog_refused "rmdir-$seed" "$opts" rmdir ) &
                pid=$!
                if wait "$pid"; then
                    echo "[rmdir diagnostic] $seed: native attestation passed"
                else
                    echo "[rmdir diagnostic] $seed: native attestation failed"
                fi
            done
        fi
        exit 1
    fi
    {
        printf 'image sha256: %s\n' "$(sha256sum "$copy" | awk '{ print $1 }')"
        btrfs inspect-internal dump-super "$copy"
        btrfs inspect-internal dump-tree -t 18446744073709551610 "$copy"
    } > "$out.logdump.partial"
    log_tree=$(python3 - "$out.logdump.partial" <<'PY2'
import re, sys
dump = open(sys.argv[1]).read()
roots = re.findall(r'\bbytenr (\d+)\b', dump.split('item 0 key (TREE_LOG', 1)[1])
print(roots[0])
PY2
    )
    btrfs inspect-internal dump-tree --follow -b "$log_tree" "$copy" >> "$out.logdump.partial"
    python3 - "$out.logdump.partial" "$kind" "$copy" <<'PY2'
import re, sys
dump, kind = open(sys.argv[1]).read(), sys.argv[2]
want = {
    'mixedlog': r'DIR_INDEX|DIR_ITEM',
    'compressedinline': r'type 0 \(inline\)[\s\S]*compression [1-3]|compression [1-3]',
    'externalextent': r'type 1 \(regular\)|extent data disk byte [1-9]',
    'sizechange': r'inline extent data size 4[0-9]',
    'xattr': r'XATTR_ITEM',
    'subvol': r'TREE_LOG ROOT_ITEM 25[6-9]',
    'newdir': r'location key \(\d+ INODE_ITEM 0\) type DIR',
    'symlink': r'location key \(\d+ INODE_ITEM 0\) type SYMLINK',
    'special': r'location key \(\d+ INODE_ITEM 0\) type CHRDEV',
    'hardlink': r'INODE_REF 256\)[^\n]*\n\s+index \d+ namelen 1 name: f\n\s+index \d+ namelen 1 name: g\n',
    'extref': r'INODE_EXTREF \d+\) itemoff \d+ itemsize \d+\n\s+index \d+ parent 256 namelen 255 name: (064){85}\n',
    'dirlink': r'INODE_REF 256\)[^\n]*\n\s+index \d+ namelen 1 name: f\n\s+index \d+ namelen 1 name: g\n',
    'nbytes': r'size 33 nbytes (?!0\n|33\n)\d+\n',
    'extrefexists': r'INODE_REF (?!256\))\d+\)[^\n]*\n\s+index \d+ namelen 1 name: g\n',
    'extrefcreated': r'INODE_REF 256\)[^\n]*\n\s+index \d+ namelen 1 name: n\n',
    'createdlink': r'INODE_REF 256\)[^\n]*\n\s+index \d+ namelen 1 name: n\n\s+index \d+ namelen 1 name: m\n',
    'extrefremoved': r'key \(256 DIR_LOG_INDEX \d+\) itemoff \d+ itemsize \d+\n\s+dir log end \d+\n',
    'compressedcut': r'EXTENT_DATA 32768\)',
    'rmdir': r'key \(256 DIR_LOG_INDEX \d+\) itemoff \d+ itemsize \d+\n\s+dir log end \d+\n',
    'linkremoved': r'key \(256 DIR_LOG_INDEX \d+\) itemoff \d+ itemsize \d+\n\s+dir log end \d+\n',
}[kind]
assert re.search(want, dump), (kind, 'native dump does not show the unsupported shape')
if kind == 'symlink':
    # The target itself is logged, uncompressed and inline: 18 bytes.
    assert re.search(r'inline extent data size 18 ram_bytes 18 compression 0\b', dump), \
        (kind, 'native dump shows no inline extent holding the link target')
if kind == 'special':
    # Each node is named by a typed entry, and its inode item carries its
    # type in the mode, printed in octal, and for a device the device
    # number as the kernel keeps it: major << 20 | minor, so 240:300 and
    # 241:5. Under the umask of 022 the socket is 755, the others 644.
    for ftype in ('CHRDEV', 'BLKDEV', 'FIFO', 'SOCK'):
        assert re.search(r'location key \(\d+ INODE_ITEM 0\) type %s\b' % ftype, dump), \
            (kind, 'native dump shows no logged entry of type', ftype)
    for mode, rdev in (('20644', 251658540), ('60644', 252706821), ('10644', 0), ('140755', 0)):
        assert re.search(r'mode %s links 1 uid \d+ gid \d+ rdev %d\b' % (mode, rdev), dump), \
            (kind, 'native dump shows no logged inode of mode and rdev', mode, rdev)
if kind == 'hardlink':
    # Each inode is logged with one reference to / holding both its
    # names, and a link count of 2: f in full, at the generation it was
    # committed with, older than the transaction that logged it; n as
    # created in that transaction, with its 19 bytes. The entries of /
    # the log speaks for name g, n and m, each the inode its reference
    # gives it.
    def logged(a, b, size):
        ref = re.search(r'key \((\d+) INODE_REF 256\) itemoff \d+ itemsize \d+\n'
                        r'\s+index \d+ namelen 1 name: %s\n\s+index \d+ namelen 1 name: %s\n'
                        % (a, b), dump)
        assert ref, (kind, 'native dump shows no reference naming both', a, b)
        ino = ref.group(1)
        item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                         r'\s+generation (\d+) transid (\d+) size %d nbytes \d+\n'
                         r'\s+block group \d+ mode 100644 links (\d+) ' % (ino, size), dump)
        assert item, (kind, 'native dump shows no logged inode item of size', size, 'for', a)
        return ino, tuple(map(int, item.groups()))
    f, (generation, transid, links) = logged('f', 'g', 33)
    assert links == 2 and 0 < generation < transid, \
        (kind, 'f is not logged in full as a committed inode with two links', generation, transid, links)
    n, (generation, transid, links) = logged('n', 'm', 19)
    assert links == 2 and generation == transid, \
        (kind, 'n is not logged as created with two links', generation, transid, links)
    for name, ino in (('g', f), ('n', n), ('m', n)):
        assert re.search(r'key \(256 DIR_INDEX \d+\) itemoff \d+ itemsize \d+\n'
                         r'\s+location key \(%s INODE_ITEM 0\) type FILE\n[^\n]*\n\s+name: %s\n'
                         % (ino, name), dump), \
            (kind, 'native dump shows no logged entry of / naming', name)
if kind == 'extref':
    # f is logged at the generation it was committed with, older than the
    # transaction that logged it, with 66 links: its INODE_REF to / and
    # its INODE_EXTREF items, whose records name / as the parent and hold
    # exactly the three names committed past the full INODE_REF and the
    # one given after the commit. The entry of / the log speaks for names
    # the new one, f's inode, inside a logged index range.
    ref = re.search(r'key \((\d+) INODE_REF 256\) itemoff \d+ itemsize \d+\n'
                    r'\s+index \d+ namelen 1 name: f\n', dump)
    assert ref, (kind, 'native dump shows no logged reference of f to /')
    f = ref.group(1)
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation (\d+) transid (\d+) size 33 nbytes \d+\n'
                     r'\s+block group \d+ mode 100644 links (\d+) ' % f, dump)
    assert item, (kind, 'native dump shows no logged inode item of f')
    generation, transid, links = map(int, item.groups())
    assert links == 66 and 0 < generation < transid, \
        (kind, 'f is not logged as a committed inode with 66 links', generation, transid, links)
    records = []
    for body in re.findall(r'key \(%s INODE_EXTREF \d+\) itemoff \d+ itemsize \d+\n'
                           r'((?:\s+index \d+ parent \d+ namelen \d+ name: [^\n]*\n)+)' % f, dump):
        records += re.findall(r'index (\d+) parent (\d+) namelen \d+ name: ([^\n]*)\n', body)
    names = sorted(name for _, _, name in records)
    assert names == [('%03d' % i) * 85 for i in range(61, 65)], \
        (kind, 'f is not logged with extended references naming exactly 061 to 064', names)
    assert all(parent == '256' for _, parent, _ in records), \
        (kind, 'an extended reference of f names a parent other than /', records)
    index = int(next(i for i, _, name in records if name == '064' * 85))
    assert re.search(r'key \(256 DIR_INDEX %d\) itemoff \d+ itemsize \d+\n'
                     r'\s+location key \(%s INODE_ITEM 0\) type FILE\n[^\n]*\n\s+name: (064){85}\n'
                     % (index, f), dump), \
        (kind, 'native dump shows no logged entry of / naming the new name at its index', index)
    ranges = [(int(a), int(b)) for a, b in re.findall(
        r'key \(256 DIR_LOG_INDEX (\d+)\) itemoff \d+ itemsize \d+\n\s+dir log end (\d+)\n', dump)]
    assert any(a <= index <= b for a, b in ranges), \
        (kind, 'no logged index range of / holds the new name', index, ranges)
if kind == 'dirlink':
    # f is logged only to say it exists: generation 0, two links, no data,
    # and one reference to / holding f at its committed index and g at a
    # later one. The entry of / naming g, f's inode, is inside a logged
    # index range of /; f's committed index is outside every one, and the
    # log holds no entry of / there.
    ref = re.search(r'key \((\d+) INODE_REF 256\) itemoff \d+ itemsize \d+\n'
                    r'\s+index (\d+) namelen 1 name: f\n\s+index (\d+) namelen 1 name: g\n', dump)
    assert ref, (kind, 'native dump shows no logged reference of f to / naming f and g')
    f, at_f, at_g = ref.group(1), int(ref.group(2)), int(ref.group(3))
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation (\d+) transid \d+ size \d+ nbytes \d+\n'
                     r'\s+block group \d+ mode 100644 links (\d+) ' % f, dump)
    assert item, (kind, 'native dump shows no logged inode item of f')
    generation, links = map(int, item.groups())
    assert (generation, links) == (0, 2), \
        (kind, 'f is not logged as an inode that exists with two links', generation, links)
    assert not re.search(r'key \(%s EXTENT_DATA ' % f, dump), (kind, 'the log holds data of f')
    assert re.search(r'key \(256 DIR_INDEX %d\) itemoff \d+ itemsize \d+\n'
                     r'\s+location key \(%s INODE_ITEM 0\) type FILE\n[^\n]*\n\s+name: g\n'
                     % (at_g, f), dump), \
        (kind, 'native dump shows no logged entry of / naming g at its index', at_g)
    assert not re.search(r'key \(256 DIR_INDEX %d\)' % at_f, dump), \
        (kind, 'the log holds an entry of / at the committed index of f', at_f)
    ranges = [(int(a), int(b)) for a, b in re.findall(
        r'key \(256 DIR_LOG_INDEX (\d+)\) itemoff \d+ itemsize \d+\n\s+dir log end (\d+)\n', dump)]
    assert any(a <= at_g <= b for a, b in ranges), \
        (kind, 'no logged index range of / holds g', at_g, ranges)
    assert not any(a <= at_f <= b for a, b in ranges), \
        (kind, 'a logged index range of / holds the committed name f', at_f, ranges)
if kind == 'nbytes':
    # f is logged in full: at the generation it was committed with, older
    # than the transaction that logged it, with its 65 links and its
    # rewritten 33 bytes in one plain inline extent of that transaction.
    # Its logged nbytes is neither 0 nor the 33 bytes committed: stale
    # bytes the kernel's own replay does not read. Only f was fsynced, so
    # the log holds no entry or index range of a directory.
    ext = re.search(r'key \((\d+) INODE_EXTREF \d+\) itemoff \d+ itemsize \d+\n'
                    r'\s+index \d+ parent 256 namelen 255 name: ', dump)
    assert ext, (kind, 'native dump shows no logged extended reference of f to /')
    f = ext.group(1)
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation (\d+) transid (\d+) size 33 nbytes (\d+)\n'
                     r'\s+block group \d+ mode 100644 links (\d+) ' % f, dump)
    assert item, (kind, 'native dump shows no logged inode item of f')
    generation, transid, nbytes, links = map(int, item.groups())
    assert links == 65 and 0 < generation < transid, \
        (kind, 'f is not logged as a committed inode with 65 links', generation, transid, links)
    assert nbytes not in (0, 33), (kind, 'f is logged with a counted nbytes', nbytes)
    assert re.search(r'key \(%s EXTENT_DATA 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation %d type 0 \(inline\)\n'
                     r'\s+inline extent data size 33 ram_bytes 33 compression 0\b'
                     % (f, transid), dump), \
        (kind, 'native dump shows no logged inline extent of the 33 rewritten bytes')
    assert not re.search(r'key \(\d+ (DIR_ITEM|DIR_INDEX|DIR_LOG_ITEM|DIR_LOG_INDEX) ', dump), \
        (kind, 'the log holds an entry or index range of a directory')
if kind == 'extrefexists':
    # f is logged only to say it exists: generation 0, 66 links, no data,
    # its INODE_REF items to / and to d, and no INODE_EXTREF item at all,
    # so the three committed extended references are the replay's to
    # keep. The entry of d naming g, f's inode, is inside a logged index
    # range of d, and the log holds no entry or index range of /.
    ref = re.search(r'key \((\d+) INODE_REF 256\) itemoff \d+ itemsize \d+\n'
                    r'\s+index \d+ namelen 1 name: f\n', dump)
    assert ref, (kind, 'native dump shows no logged reference of f to /')
    f = ref.group(1)
    sub = re.search(r'key \((\d+) INODE_REF 256\) itemoff \d+ itemsize \d+\n'
                    r'\s+index \d+ namelen 1 name: d\n', dump)
    assert sub, (kind, 'native dump shows no logged reference of d to /')
    dino = sub.group(1)
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation (\d+) transid \d+ size \d+ nbytes \d+\n'
                     r'\s+block group \d+ mode 100644 links (\d+) ' % f, dump)
    assert item, (kind, 'native dump shows no logged inode item of f')
    generation, links = map(int, item.groups())
    assert (generation, links) == (0, 66), \
        (kind, 'f is not logged as an inode that exists with 66 links', generation, links)
    assert not re.search(r'key \(\d+ INODE_EXTREF ', dump), \
        (kind, 'the log holds an extended reference')
    assert not re.search(r'key \(%s EXTENT_DATA ' % f, dump), (kind, 'the log holds data of f')
    at_g = re.search(r'key \(%s INODE_REF %s\) itemoff \d+ itemsize \d+\n'
                     r'\s+index (\d+) namelen 1 name: g\n' % (f, dino), dump)
    assert at_g, (kind, 'native dump shows no logged reference of f to d naming g')
    at_g = int(at_g.group(1))
    assert re.search(r'key \(%s DIR_INDEX %d\) itemoff \d+ itemsize \d+\n'
                     r'\s+location key \(%s INODE_ITEM 0\) type FILE\n[^\n]*\n\s+name: g\n'
                     % (dino, at_g, f), dump), \
        (kind, 'native dump shows no logged entry of d naming g at its index', at_g)
    ranges = [(int(a), int(b)) for a, b in re.findall(
        r'key \(%s DIR_LOG_INDEX (\d+)\) itemoff \d+ itemsize \d+\n\s+dir log end (\d+)\n'
        % dino, dump)]
    assert any(a <= at_g <= b for a, b in ranges), \
        (kind, 'no logged index range of d holds g', at_g, ranges)
    assert not re.search(r'key \(256 (DIR_ITEM|DIR_INDEX|DIR_LOG_ITEM|DIR_LOG_INDEX) ', dump), \
        (kind, 'the log holds an entry or index range of /')
if kind == 'extrefcreated':
    # n, which the log creates, is logged only to say it exists:
    # generation 0, size 0, 65 links and no data, its INODE_REF to / and
    # three INODE_EXTREF items, whose records are 65 names between them.
    # Every one of them is an entry of / naming n, inside a logged index
    # range of /, and the log holds no other entry of / and no other
    # extended reference.
    ref = re.search(r'key \((\d+) INODE_REF 256\) itemoff \d+ itemsize \d+\n'
                    r'([ \t]+index \d+ namelen 1 name: n\n'
                    r'(?:[ \t]+index \d+ namelen \d+ name: [^\n]*\n)*)', dump)
    assert ref, (kind, 'native dump shows no logged reference of n to /')
    n = ref.group(1)
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation (\d+) transid \d+ size (\d+) nbytes \d+\n'
                     r'\s+block group \d+ mode 100644 links (\d+) ' % n, dump)
    assert item, (kind, 'native dump shows no logged inode item of n')
    generation, size, links = map(int, item.groups())
    assert (generation, size, links) == (0, 0, 65), \
        (kind, 'n is not logged as an inode that exists with 65 links', generation, size, links)
    assert not re.search(r'key \(%s EXTENT_DATA ' % n, dump), (kind, 'the log holds data of n')
    extrefs = re.findall(r'key \(%s INODE_EXTREF \d+\) itemoff \d+ itemsize \d+\n'
                         r'((?:[ \t]+index \d+ parent 256 namelen 255 name: [^\n]*\n)+)' % n, dump)
    assert len(extrefs) == 3, (kind, 'n is not logged with three INODE_EXTREF items', len(extrefs))
    assert len(re.findall(r'key \(\d+ INODE_EXTREF \d+\) itemoff ', dump)) == 3, \
        (kind, 'the log holds an extended reference of another inode')
    named = len(re.findall(r'index \d+ namelen', ref.group(2)))
    extended = sum(len(re.findall(r'index \d+ parent', e)) for e in extrefs)
    assert named + extended == 65, (kind, 'the references of n are not its 65 names', named, extended)
    entries = [int(i) for i in re.findall(
        r'key \(256 DIR_INDEX (\d+)\) itemoff \d+ itemsize \d+\n'
        r'\s+location key \(%s INODE_ITEM 0\) type FILE\n' % n, dump)]
    assert len(entries) == 65 == len(re.findall(r'key \(256 DIR_INDEX \d+\) itemoff ', dump)), \
        (kind, 'the logged entries of / are not the 65 names of n', len(entries))
    ranges = [(int(a), int(b)) for a, b in re.findall(
        r'key \(256 DIR_LOG_INDEX (\d+)\) itemoff \d+ itemsize \d+\n\s+dir log end (\d+)\n', dump)]
    assert all(any(a <= i <= b for a, b in ranges) for i in entries), \
        (kind, 'an entry of / naming n is outside every logged index range', ranges)
if kind == 'createdlink':
    # n, which the log creates, is logged only to say it exists:
    # generation 0, size 0, two links and no data, and one reference to /
    # holding n and m. Each name is an entry of / naming n, inside a
    # logged index range of /, and the log holds no other entry of /.
    ref = re.search(r'key \((\d+) INODE_REF 256\) itemoff \d+ itemsize \d+\n'
                    r'\s+index (\d+) namelen 1 name: n\n\s+index (\d+) namelen 1 name: m\n', dump)
    assert ref, (kind, 'native dump shows no logged reference of n to / naming n and m')
    n, at_n, at_m = ref.group(1), int(ref.group(2)), int(ref.group(3))
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation (\d+) transid \d+ size (\d+) nbytes \d+\n'
                     r'\s+block group \d+ mode 100644 links (\d+) ' % n, dump)
    assert item, (kind, 'native dump shows no logged inode item of n')
    generation, size, links = map(int, item.groups())
    assert (generation, size, links) == (0, 0, 2), \
        (kind, 'n is not logged as an inode that exists with two links', generation, size, links)
    assert not re.search(r'key \(%s EXTENT_DATA ' % n, dump), (kind, 'the log holds data of n')
    ranges = [(int(a), int(b)) for a, b in re.findall(
        r'key \(256 DIR_LOG_INDEX (\d+)\) itemoff \d+ itemsize \d+\n\s+dir log end (\d+)\n', dump)]
    for at, name in ((at_n, 'n'), (at_m, 'm')):
        assert re.search(r'key \(256 DIR_INDEX %d\) itemoff \d+ itemsize \d+\n'
                         r'\s+location key \(%s INODE_ITEM 0\) type FILE\n[^\n]*\n\s+name: %s\n'
                         % (at, n, name), dump), \
            (kind, 'native dump shows no logged entry of / naming %s at its index' % name, at)
        assert any(a <= at <= b for a, b in ranges), \
            (kind, 'no logged index range of / holds %s' % name, at, ranges)
    assert len(re.findall(r'key \(256 DIR_INDEX \d+\) itemoff ', dump)) == 2, \
        (kind, 'the log holds an entry of / other than n and m')
if kind == 'extrefremoved':
    # Committed, f has 33 bytes, one link, no INODE_REF item and one
    # INODE_EXTREF item, whose one record is its last long name in /,
    # named by an entry of / at that record's index. The log does not
    # hold f or any entry naming it, holds no extended reference, and
    # holds an index range of / covering that index with no entry there.
    import subprocess
    tree = subprocess.run(['btrfs', 'inspect-internal', 'dump-tree', '-t', '5', sys.argv[3]],
                          check=True, capture_output=True, text=True).stdout
    ext = re.findall(r'key \((\d+) INODE_EXTREF \d+\) itemoff \d+ itemsize \d+\n'
                     r'((?:[ \t]+index \d+ parent \d+ namelen \d+ name: [^\n]*\n)+)', tree)
    assert len(ext) == 1, (kind, 'the committed trees do not hold one extended reference', len(ext))
    f, records = ext[0]
    named = re.findall(r'index (\d+) parent (\d+) namelen \d+ name: ([^\n]*)\n', records)
    assert len(named) == 1 and named[0][1:] == ('256', '063' * 85), \
        (kind, 'the extended reference of f is not one record of its last name in /', named)
    at = int(named[0][0])
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation \d+ transid \d+ size (\d+) nbytes \d+\n'
                     r'\s+block group \d+ mode 100644 links (\d+) ' % f, tree)
    assert item and item.groups() == ('33', '1'), \
        (kind, 'f is not committed with 33 bytes and one link', item and item.groups())
    assert not re.search(r'key \(%s INODE_REF ' % f, tree), (kind, 'f is committed with an INODE_REF item')
    assert re.search(r'key \(256 DIR_INDEX %d\) itemoff \d+ itemsize \d+\n'
                     r'\s+location key \(%s INODE_ITEM 0\) type FILE\n' % (at, f), tree), \
        (kind, 'no committed entry of / names f at the index of its extended reference', at)
    assert not re.search(r'\bkey \(%s ' % f, dump), (kind, 'the log holds f or an entry naming it')
    assert not re.search(r'key \(\d+ INODE_EXTREF ', dump), (kind, 'the log holds an extended reference')
    assert not re.search(r'key \(256 DIR_INDEX %d\) ' % at, dump), \
        (kind, 'the log holds an entry of / at the removed index', at)
    ranges = [(int(a), int(b)) for a, b in re.findall(
        r'key \(256 DIR_LOG_INDEX (\d+)\) itemoff \d+ itemsize \d+\n\s+dir log end (\d+)\n', dump)]
    assert any(a <= at <= b for a, b in ranges), \
        (kind, 'no logged index range of / holds the removed index', at, ranges)
if kind == 'rmdir':
    # Committed, / names one directory, e, empty with one link. The log
    # does not hold e or any entry naming it, and holds an index range of
    # / covering e's index with no entry there.
    import subprocess
    tree = subprocess.run(['btrfs', 'inspect-internal', 'dump-tree', '-t', '5', sys.argv[3]],
                          check=True, capture_output=True, text=True).stdout
    named = re.findall(r'key \(256 DIR_INDEX (\d+)\) itemoff \d+ itemsize \d+\n'
                       r'\s+location key \((\d+) INODE_ITEM 0\) type DIR\n[^\n]*\n\s+name: e\n', tree)
    assert len(named) == 1, (kind, 'the committed / does not name one directory e', named)
    at, e = int(named[0][0]), named[0][1]
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation \d+ transid \d+ size (\d+) nbytes \d+\n'
                     r'\s+block group \d+ mode 40\d+ links (\d+) ' % e, tree)
    assert item and item.groups() == ('0', '1'), \
        (kind, 'e is not committed as an empty directory with one link', item and item.groups())
    assert not re.search(r'\bkey \(%s ' % e, dump), (kind, 'the log holds e or an item of it')
    assert not re.search(r'key \(256 DIR_INDEX %d\) ' % at, dump), \
        (kind, 'the log holds an entry of / at the removed index', at)
    ranges = [(int(a), int(b)) for a, b in re.findall(
        r'key \(256 DIR_LOG_INDEX (\d+)\) itemoff \d+ itemsize \d+\n\s+dir log end (\d+)\n', dump)]
    assert any(a <= at <= b for a, b in ranges), \
        (kind, 'no logged index range of / holds the removed index', at, ranges)
if kind == 'linkremoved':
    # Committed, / names one regular file twice, f and g, with two links
    # and both names in one INODE_REF. The log does not hold that inode or
    # any entry naming it, and holds an index range of / covering g's
    # index with no entry there.
    import subprocess
    tree = subprocess.run(['btrfs', 'inspect-internal', 'dump-tree', '-t', '5', sys.argv[3]],
                          check=True, capture_output=True, text=True).stdout
    named = {n: (int(i), o) for i, o, n in re.findall(
        r'key \(256 DIR_INDEX (\d+)\) itemoff \d+ itemsize \d+\n'
        r'\s+location key \((\d+) INODE_ITEM 0\) type FILE\n[^\n]*\n\s+name: ([fg])\n', tree)}
    assert set(named) == {'f', 'g'} and named['f'][1] == named['g'][1], \
        (kind, 'the committed / does not name one file as f and g', named)
    at, f = named['g'][0], named['f'][1]
    item = re.search(r'key \(%s INODE_ITEM 0\) itemoff \d+ itemsize \d+\n'
                     r'\s+generation \d+ transid \d+ size \d+ nbytes \d+\n'
                     r'\s+block group \d+ mode 100\d+ links (\d+) ' % f, tree)
    assert item and item.group(1) == '2', \
        (kind, 'f is not committed with two links', item and item.group(1))
    refs = re.findall(r'key \(%s INODE_REF 256\) itemoff \d+ itemsize \d+\n'
                      r'((?:\s+index \d+ namelen \d+ name: \S+\n)+)' % f, tree)
    assert len(refs) == 1 and sorted(re.findall(r'name: (\S+)', refs[0])) == ['f', 'g'], \
        (kind, 'f is not committed with one INODE_REF holding f and g', refs)
    assert not re.search(r'\bkey \(%s ' % f, dump), (kind, 'the log holds f or an item of it')
    assert not re.search(r'key \(256 DIR_INDEX %d\) ' % at, dump), \
        (kind, 'the log holds an entry of / at the removed index', at)
    ranges = [(int(a), int(b)) for a, b in re.findall(
        r'key \(256 DIR_LOG_INDEX (\d+)\) itemoff \d+ itemsize \d+\n\s+dir log end (\d+)\n', dump)]
    assert any(a <= at <= b for a, b in ranges), \
        (kind, 'no logged index range of / holds the removed index', at, ranges)
if kind == 'compressedinline':
    # The native tool must print a compressed inline extent: nonzero
    # compression on the inline line itself, stored smaller than its bytes.
    inline = [(int(n), int(r)) for n, r in re.findall(
        r'inline extent data size (\d+) ram_bytes (\d+) compression [1-3]\b', dump)]
    assert any(n < r for n, r in inline), (kind, 'native dump shows no compressed inline extent', inline)
if kind == 'compressedcut':
    # The logged write is its own uncompressed sector at 32768.
    logged = re.search(r'EXTENT_DATA 32768\)[\s\S]*?extent compression (\d+)', dump)
    assert logged and logged.group(1) == '0', (kind, 'native dump shows no plain logged sector at 32768')
PY2
    if [ "$kind" = compressedcut ]; then
        # The committed file is one compressed extent, read from the
        # native dump of the committed subvolume tree, not ours.
        btrfs inspect-internal dump-tree -t 5 "$copy" | grep -Ec 'extent compression [1-3] ' >/dev/null || {
            echo "guest-build-images: compressedcut committed no compressed extent" >&2
            exit 1
        }
    fi
    mv -f "$out.logdump.partial" "$out.logdump"
    cp --sparse=always "$copy" "$out.img.partial"
    mv -f "$out.img.partial" "$out.img"
    note "built btrfs-dirty-log-$shape (log_root $log_root; native dump attests $kind)"
}

# ---------------------------------------------------------------------
# dirtylog/btrfs-dirty-log-replay — a log tree holding every kind of
# change a replay folds back in (#266): data and names.
#
# Committed by a sync, then changed and made durable by fsyncs alone, the
# image copied while mounted as `build_dirtylog` does:
#
# - big.bin, 256 KiB: 4 KiB overwritten in the middle, so the log holds
#   one new extent inside a committed one that replay has to split;
# - new.bin: created with 128 KiB, a new extent and its checksums;
# - grow.bin: 8 KiB appended to 8 KiB, a new extent after a committed one;
# - shrink.bin: 64 KiB truncated to 10000 bytes;
# - d/: gone.bin removed (its last name, so its data goes too),
#   old-name.txt renamed to new-name.txt, fresh.txt created and fsynced,
#   and then the directory fsynced, so the log holds its names and the
#   index ranges they speak for.
#
# The directory goes first: if any fsync here fell back to a full commit,
# everything logged before it would be committed instead, and the
# data changes after it would still be in the log. The test reads the
# log and names any change missing from it, rather than finding nothing
# to replay.
# ---------------------------------------------------------------------
# Emit a finite yes-style payload: yes | head terminates yes with SIGPIPE,
# which aborts this script under pipefail despite writing the right bytes.
repeat_bytes() {
    python3 -c 'import sys
pattern = (sys.argv[1] + "\n").encode()
size = int(sys.argv[2])
sys.stdout.buffer.write((pattern * (size // len(pattern) + 1))[:size])' "$1" "$2"
}

build_dirtylog_replay() {
    local img="$WORK/btrfs-dirty-log-replay.img" copy="$WORK/btrfs-dirty-log-replay-copy.img"
    local log_root
    rm -f "$img" "$copy"
    truncate -s 256M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop,commit=300 "$img" "$MNT"
    repeat_bytes 'committed big' 262144 > "$MNT/big.bin"
    repeat_bytes 'committed grow' 8192 > "$MNT/grow.bin"
    repeat_bytes 'committed shrink' 65536 > "$MNT/shrink.bin"
    mkdir "$MNT/d"
    printf 'kept\n' > "$MNT/d/keep.txt"
    repeat_bytes 'committed gone' 12288 > "$MNT/d/gone.bin"
    printf 'renamed\n' > "$MNT/d/old-name.txt"
    sync

    rm "$MNT/d/gone.bin"
    mv "$MNT/d/old-name.txt" "$MNT/d/new-name.txt"
    printf 'fresh\n' > "$MNT/d/fresh.txt"
    fsync_path "$MNT/d/fresh.txt"
    fsync_path "$MNT/d"
    repeat_bytes 'FSYNCED BIG' 4096 |
        dd of="$MNT/big.bin" bs=4096 seek=16 conv=notrunc status=none
    fsync_path "$MNT/big.bin"
    repeat_bytes 'created new' 131072 > "$MNT/new.bin"
    fsync_path "$MNT/new.bin"
    repeat_bytes 'appended grow' 8192 >> "$MNT/grow.bin"
    fsync_path "$MNT/grow.bin"
    truncate -s 10000 "$MNT/shrink.bin"
    fsync_path "$MNT/shrink.bin"

    cp --sparse=always "$img" "$copy"
    umount "$MNT"
    rm -f "$img"
    log_root=$(btrfs inspect-internal dump-super "$copy" | awk '$1 == "log_root" { print $2 }')
    if [ -z "$log_root" ] || [ "$log_root" = 0 ]; then
        echo "guest-build-images: the dirty-log replay image has log_root '${log_root}' — the fsyncs did not leave a log tree" >&2
        exit 1
    fi
    # The native tools' own view of this exact crash image, taken before
    # it leaves the guest: the image's sha256, its superblock, the log
    # root tree and the log tree that root tree points at.
    {
        printf 'image sha256: %s\n' "$(sha256sum "$copy" | awk '{ print $1 }')"
        btrfs inspect-internal dump-super "$copy"
        btrfs inspect-internal dump-tree -t 18446744073709551610 "$copy"
    } > "$OUT/dirtylog/btrfs-dirty-log-replay.logdump.partial"
    log_tree=$(python3 - "$OUT/dirtylog/btrfs-dirty-log-replay.logdump.partial" <<'PY'
import re, sys
dump = open(sys.argv[1]).read()
keys = re.findall(r'item \d+ key \((\S+) (\S+) (\S+)\)', dump)
assert keys == [('TREE_LOG', 'ROOT_ITEM', '5')], ('unexpected log roots', keys)
roots = re.findall(r'\bbytenr (\d+)\b', dump.split('item 0 key (TREE_LOG', 1)[1])
assert roots and int(roots[0]) > 0, ('missing log tree block', roots)
print(roots[0])
PY
    )
    btrfs inspect-internal dump-tree --follow -b "$log_tree" "$copy" \
        >> "$OUT/dirtylog/btrfs-dirty-log-replay.logdump.partial"
    mv -f "$OUT/dirtylog/btrfs-dirty-log-replay.logdump.partial" \
        "$OUT/dirtylog/btrfs-dirty-log-replay.logdump"
    cp --sparse=always "$copy" "$OUT/dirtylog/btrfs-dirty-log-replay.img.partial"
    mv -f "$OUT/dirtylog/btrfs-dirty-log-replay.img.partial" \
        "$OUT/dirtylog/btrfs-dirty-log-replay.img"
    rm -f "$copy"
    note "built btrfs-dirty-log-replay (log_root $log_root)"
}

# ---------------------------------------------------------------------
# cli — the volume the command-line tools are tested against: labelled,
# and holding one of every shape a user will point `fs.btrfs` at.
#
#   hello.txt           a few bytes, so it is stored inline in its item
#   dir/random.bin      300 KiB of noise: an ordinary extent
#   dir/sub/deep.txt    a file two directories down
#   link                a symlink to hello.txt
#   sparse.bin          a 256 KiB hole and then 4 KiB of data
#   zstd/text.txt       compressible text under `compression=zstd`
#   nocow/data.bin      64 KiB in a `chattr +C` directory: NODATACOW, the
#                       one kind of file this crate can overwrite in place
#   vol/                a subvolume, with inside.txt, and later.txt written
#                       after the snapshot below was taken
#   snap/               a read-only snapshot of vol/, so it lacks later.txt
#
# THE MANIFEST IS WHAT THE KERNEL SAYS IS THERE, read back through its own
# driver on a read-only mount: one line per path, tab-separated --
# `<f|d|l> <path> <size> <sha256|target|-> <subvol|->` -- so the tools'
# listings and bytes are compared against Linux rather than against this
# crate. Each shape the list promises is checked on the built image, and a
# build where one did not happen (no inline extent, no zstd, no hole, no
# NODATACOW flag) fails here rather than in a test that would read it as a
# pass. Its own directory, like the snapshot fixture's: the suites that walk
# every image in test-disks/ assume what an ordinary volume holds.
# ---------------------------------------------------------------------
build_cli() {
    local img="$WORK/btrfs-cli.img" manifest="$WORK/btrfs-cli.manifest" dump="$WORK/cli-tree.txt"
    rm -f "$img" "$manifest"
    truncate -s 512M "$img"
    mkfs.btrfs -f -L CLITEST "$img" >/dev/null
    mount -o loop "$img" "$MNT"
    printf 'hello from the kernel\n' > "$MNT/hello.txt"
    mkdir -p "$MNT/dir/sub"
    dd if=/dev/urandom of="$MNT/dir/random.bin" bs=1024 count=300 status=none
    python3 -c "print('\n'.join('line %d of a file two directories down' % i for i in range(500)))" \
        > "$MNT/dir/sub/deep.txt"
    ln -s hello.txt "$MNT/link"
    dd if=/dev/urandom of="$MNT/sparse.bin" bs=4096 count=1 seek=64 status=none
    mkdir "$MNT/zstd"
    btrfs property set "$MNT/zstd" compression zstd
    python3 -c "print('the quick brown fox jumps over the lazy dog ' * 4000)" > "$MNT/zstd/text.txt"
    mkdir "$MNT/nocow"
    chattr +C "$MNT/nocow"
    dd if=/dev/urandom of="$MNT/nocow/data.bin" bs=4096 count=16 status=none
    btrfs subvolume create "$MNT/vol" >/dev/null
    echo "inside the subvolume" > "$MNT/vol/inside.txt"
    sync
    btrfs subvolume snapshot -r "$MNT/vol" "$MNT/snap" >/dev/null
    echo "written after the snapshot" > "$MNT/vol/later.txt"
    lsattr "$MNT/nocow/data.bin" | grep -q '^[^ ]*C' || {
        echo "guest-build-images: btrfs-cli's nocow/data.bin is not NODATACOW -- chattr +C did not take" >&2
        umount "$MNT"
        exit 1
    }
    sync
    umount "$MNT"

    mount -o loop,ro "$img" "$MNT"
    ( cd "$MNT"
      find . -mindepth 1 | sort | while read -r p; do
          path="${p#.}"
          subvol=-
          if [ -d "$p" ] && [ ! -L "$p" ] && [ "$(stat -c %i "$p")" = 256 ]; then subvol=subvol; fi
          if [ -L "$p" ]; then
              printf 'l\t%s\t%s\t%s\t-\n' "$path" "$(stat -c %s "$p")" "$(readlink "$p")"
          elif [ -d "$p" ]; then
              printf 'd\t%s\t-\t-\t%s\n' "$path" "$subvol"
          else
              printf 'f\t%s\t%s\t%s\t-\n' "$path" "$(stat -c %s "$p")" "$(sha256sum "$p" | cut -d' ' -f1)"
          fi
      done
    ) > "$manifest"
    # The hole is a hole: fewer bytes allocated than the file's length.
    [ "$(( $(stat -c %b "$MNT/sparse.bin") * 512 ))" -lt "$(stat -c %s "$MNT/sparse.bin")" ] || {
        echo "guest-build-images: btrfs-cli's sparse.bin has no hole" >&2
        umount "$MNT"
        exit 1
    }
    umount "$MNT"
    grep -q $'^d\t/vol\t-\t-\tsubvol$' "$manifest" && grep -q $'^d\t/snap\t-\t-\tsubvol$' "$manifest" || {
        echo "guest-build-images: btrfs-cli's manifest does not show vol/ and snap/ as subvolumes" >&2
        exit 1
    }

    btrfs inspect-internal dump-tree -t 5 "$img" > "$dump"
    grep -q 'inline extent' "$dump" || {
        echo "guest-build-images: btrfs-cli has no inline extent (hello.txt)" >&2
        exit 1
    }
    grep -q 'extent compression 3 (zstd)' "$dump" || {
        echo "guest-build-images: btrfs-cli has no zstd-compressed extent (zstd/text.txt)" >&2
        exit 1
    }
    rm -f "$dump"
    dump_super "$img"
    mkdir -p "$OUT/cli"
    local f
    for f in "$img" "${img%.img}.superdump" "$manifest"; do
        cp --sparse=always "$f" "$OUT/cli/$(basename "$f").partial"
        mv -f "$OUT/cli/$(basename "$f").partial" "$OUT/cli/$(basename "$f")"
        rm -f "$f"
    done
    note "built btrfs-cli"
}

# ---------------------------------------------------------------------
# features — one filesystem per optional on-disk feature, each made by
# the kernel with content and a manifest of it (#270).
#
#   btrfs-bgt   the block group tree (`-O block-group-tree`): block group
#               items live in a tree of their own instead of the extent
#               tree, so a reader that looks for them in the extent tree
#               finds none. The manifest is every file's size and SHA-256.
#               Formatted by the static btrfs-progs, whose mkfs knows the
#               feature; filled by the kernel, and checked by Debian's
#               `btrfs check`.
#
# In its own directory: the suites that walk every image in test-disks/
# also try to write each one, and a block group tree volume is refused
# for writing on purpose.
# ---------------------------------------------------------------------
build_features() {
    local img="$WORK/btrfs-bgt.img" manifest="$WORK/btrfs-bgt.manifest" i
    rm -f "$img" "$manifest"
    truncate -s 512M "$img"
    "$STATIC_PROGS/mkfs.btrfs" -f -O block-group-tree "$img" >/dev/null
    mount -o loop "$img" "$MNT"
    mkdir -p "$MNT/dir"
    for i in 1 2 3; do
        echo "bgt file $i" > "$MNT/dir/file-$i.txt"
    done
    dd if=/dev/urandom of="$MNT/big.bin" bs=1M count=4 status=none
    sync
    ( cd "$MNT" && find . -mindepth 1 -type f | sort | while read -r p; do
        printf '%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" \
            "$(sha256sum "$p" | cut -d' ' -f1)"
      done ) > "$manifest"
    umount "$MNT"
    btrfs inspect-internal dump-super "$img" | grep -q 'BLOCK_GROUP_TREE' || {
        echo "guest-build-images: btrfs-bgt has no block group tree" >&2
        exit 1
    }
    btrfs check --readonly "$img" >/dev/null 2>&1 || {
        echo "guest-build-images: btrfs check does not find btrfs-bgt clean" >&2
        exit 1
    }
    mkdir -p "$OUT/features"
    local f
    for f in "$img" "$manifest"; do
        cp --sparse=always "$f" "$OUT/features/$(basename "$f").partial"
        mv -f "$OUT/features/$(basename "$f").partial" "$OUT/features/$(basename "$f")"
        rm -f "$f"
    done
    note "built features/btrfs-bgt"

    build_feature_verity
    build_feature_zoned
}

# Emulated zones on a regular file retain conventional superblock
# offsets. The kernel supplies the file bytes and manifest; btrfs-progs
# supplies an independent consistency verdict before publication (#454).
build_feature_zoned() {
    local img="$WORK/btrfs-zoned.img" manifest="$WORK/btrfs-zoned.manifest" i f
    truncate -s 2G "$img"
    "$STATIC_PROGS/mkfs.btrfs" -f -d single -m single -O zoned "$img" >/dev/null
    mount -o loop "$img" "$MNT"
    mkdir -p "$MNT/dir"
    for i in 1 2 3; do
        echo "zoned file $i" > "$MNT/dir/file-$i.txt"
    done
    dd if=/dev/urandom of="$MNT/big.bin" bs=1M count=4 status=none
    : > "$MNT/empty"
    sync
    manifest_of "$MNT" > "$manifest"
    umount "$MNT"
    "$STATIC_PROGS/btrfs" inspect-internal dump-super "$img" > "$WORK/zoned.superdump"
    grep -q 'ZONED' "$WORK/zoned.superdump" || {
        echo "guest-build-images: btrfs-zoned does not carry ZONED" >&2
        exit 1
    }
    "$STATIC_PROGS/btrfs" check --readonly "$img" >/dev/null 2>&1 || {
        echo "guest-build-images: btrfs check does not find btrfs-zoned clean" >&2
        "$STATIC_PROGS/btrfs" check --readonly "$img" >&2 || true
        exit 1
    }
    mkdir -p "$OUT/features"
    for f in "$img" "$manifest"; do
        cp --sparse=always "$f" "$OUT/features/$(basename "$f").partial"
        mv -f "$OUT/features/$(basename "$f").partial" "$OUT/features/$(basename "$f")"
    done
    note "built features/btrfs-zoned"
}

# ---------------------------------------------------------------------
# features/btrfs-verity — fs-verity (#270): files sealed by the kernel,
# each with a Merkle tree over its blocks.
#
#   small.txt            one block, so no tree: the root hash is the block's
#   empty                no data: the root hash is all zeros
#   partial-block.bin    three blocks, the last one short
#   two-levels.bin       300 blocks of SHA-256: a tree of two levels
#   sha512-salted.bin    4100 blocks of SHA-512 with a salt: three levels
#   nodatasum/tamper.bin sixteen blocks with no data checksums, so the
#                        tampered copy below is caught by verity and by
#                        nothing else
#   plain.txt            not sealed
#
# The manifest is one line per file: path, size, SHA-256 of the contents,
# then what the kernel reports for a sealed file and `-` for one that is
# not -- `fsverity measure`, and the SHA-256 of the Merkle tree and of the
# descriptor as `fsverity digest` builds them, kept only when that digest
# is the kernel's measurement.
#
# btrfs-verity-tampered.img is a copy with 64 bytes of tamper.bin's second
# block overwritten. Before it is published, the kernel must read that
# file's first block from it and refuse the file whole, or the build
# fails: that is what a reader of the copy is held to.
# ---------------------------------------------------------------------
build_feature_verity() {
    local img="$WORK/btrfs-verity.img" manifest="$WORK/btrfs-verity.manifest"
    local bad="$WORK/btrfs-verity-tampered.img" p ino disk phys first whole m t d g
    local -a params
    command -v fsverity >/dev/null || {
        echo "guest-build-images.sh: fsverity (fsverity-utils) is not in the guest." >&2
        echo "  It is installed by scripts/vm-setup.sh; 'chore vm:provision' applies it again." >&2
        exit 1
    }
    rm -f "$img" "$manifest" "$bad"
    truncate -s 512M "$img"
    mkfs.btrfs -f "$img" >/dev/null
    mount -o loop "$img" "$MNT"
    echo "one block, sealed" > "$MNT/small.txt"
    : > "$MNT/empty"
    head -c 10000 /dev/urandom > "$MNT/partial-block.bin"
    dd if=/dev/urandom of="$MNT/two-levels.bin" bs=4K count=300 status=none
    dd if=/dev/urandom of="$MNT/sha512-salted.bin" bs=4K count=4100 status=none
    echo "not sealed" > "$MNT/plain.txt"
    mkdir "$MNT/nodatasum"
    chattr +C "$MNT/nodatasum"
    dd if=/dev/urandom of="$MNT/nodatasum/tamper.bin" bs=4K count=16 status=none
    sync
    for p in small.txt empty partial-block.bin two-levels.bin nodatasum/tamper.bin; do
        fsverity enable "$MNT/$p" || {
            echo "guest-build-images: the kernel would not enable fs-verity on $p" >&2
            exit 1
        }
    done
    fsverity enable --hash-alg=sha512 --salt=0123456789abcdef "$MNT/sha512-salted.bin"
    sync
    ( cd "$MNT" && find . -mindepth 1 -type f | sort | while read -r p; do
        if m=$(fsverity measure "$p" 2>/dev/null); then
            m=${m%% *}
            # The guest's kernel seals and measures btrfs files, but its
            # btrfs has no FS_IOC_READ_VERITY_METADATA (Debian 12's 6.1;
            # the ioctl reached btrfs later), so `fsverity dump_metadata`
            # answers ENOTTY for every sealed file. `fsverity digest`
            # builds the tree and the descriptor from the contents with
            # the parameters the file was sealed with, and they are kept
            # only when its digest is the kernel's measurement: that is
            # the hash of the descriptor, which carries the tree's root.
            params=()
            [ "$p" = ./sha512-salted.bin ] && params=(--hash-alg=sha512 --salt=0123456789abcdef)
            g=$(fsverity digest "${params[@]}" --out-merkle-tree="$WORK/verity.tree" \
                --out-descriptor="$WORK/verity.desc" "$p")
            [ "${g%% *}" = "$m" ] || {
                echo "guest-build-images: btrfs-verity: $p: fsverity digest ${g%% *}, the kernel measures $m" >&2
                exit 1
            }
            t=$(sha256sum < "$WORK/verity.tree" | cut -d' ' -f1)
            d=$(sha256sum < "$WORK/verity.desc" | cut -d' ' -f1)
            rm -f "$WORK/verity.tree" "$WORK/verity.desc"
        else
            m=- t=- d=-
        fi
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" \
            "$(sha256sum "$p" | cut -d' ' -f1)" "$m" "$t" "$d"
      done ) > "$manifest"
    ino=$(stat -c %i "$MNT/nodatasum/tamper.bin")
    umount "$MNT"
    # The guest's btrfs-progs 6.2 prints the raw compat_ro mask but does not
    # know the VERITY name yet. Check the upstream bit value, not its label.
    local compat_ro
    compat_ro=$(btrfs inspect-internal dump-super "$img" \
        | awk '$1 == "compat_ro_flags" { print $2 }')
    if [ -z "$compat_ro" ] || (( (compat_ro & 4) == 0 )); then
        echo "guest-build-images: btrfs-verity compat_ro_flags=${compat_ro:-missing} lacks VERITY (0x4)" >&2
        exit 1
    fi
    [ "$(grep -c 'sha256:\|sha512:' "$manifest")" -eq 6 ] || {
        echo "guest-build-images: btrfs-verity: the kernel measures $(grep -c 'sha256:\|sha512:' "$manifest") of six sealed files" >&2
        exit 1
    }
    btrfs check --readonly "$img" >/dev/null 2>&1 || {
        echo "guest-build-images: btrfs check does not find btrfs-verity clean" >&2
        btrfs check --readonly "$img" >&2 || true
        exit 1
    }

    disk=$(btrfs inspect-internal dump-tree -t 5 "$img" \
        | awk -v k="($ino EXTENT_DATA 0)" 'index($0, k) { found = 1; next }
                                           found && /extent data disk byte/ { print $5; exit }')
    phys=$(btrfs-map-logical -l $((disk + 4096)) -b 4096 "$img" 2>/dev/null \
        | awk '$1 == "mirror" && $2 == 1 { print $6; exit }')
    [ -n "$disk" ] && [ -n "$phys" ] || {
        echo "guest-build-images: btrfs-verity: tamper.bin's second block was not found" >&2
        exit 1
    }
    cp --sparse=always "$img" "$bad"
    dd if=/dev/urandom of="$bad" bs=1 seek=$((phys + 512)) count=64 conv=notrunc status=none
    btrfs device scan --forget >/dev/null 2>&1 || true
    mount -o loop,ro "$bad" "$MNT"
    first=refused whole=read
    dd if="$MNT/nodatasum/tamper.bin" of=/dev/null bs=4K count=1 status=none 2>/dev/null && first=read
    cat "$MNT/nodatasum/tamper.bin" > /dev/null 2>&1 || whole=refused
    umount "$MNT"
    btrfs device scan --forget >/dev/null 2>&1 || true
    [ "$first" = read ] && [ "$whole" = refused ] || {
        echo "guest-build-images: btrfs-verity-tampered: the kernel's first block $first, whole file $whole" >&2
        exit 1
    }
    echo "# tampered: /nodatasum/tamper.bin block 1; the kernel reads block 0 and refuses the file" >> "$manifest"

    mkdir -p "$OUT/features"
    local f
    for f in "$img" "$manifest" "$bad"; do
        cp --sparse=always "$f" "$OUT/features/$(basename "$f").partial"
        mv -f "$OUT/features/$(basename "$f").partial" "$OUT/features/$(basename "$f")"
        rm -f "$f"
    done
    note "built features/btrfs-verity and features/btrfs-verity-tampered"
}

# ---------------------------------------------------------------------
# scrub — a DUP volume with three copies damaged on purpose, and what the
# kernel's own read-only scrub said about it (#268).
#
#   the second copy of the first data sector of a.bin
#   the first copy of its second data sector
#   the second copy of the fs tree's root block
#
# Each copy is found with btrfs-map-logical and overwritten with dd while
# the volume is not mounted, then the volume is mounted read-only and
# `btrfs scrub start -B -R -r` run over it. The manifest records the
# damaged addresses (logical, and the copy as the kernel numbers them,
# from 1), the scrub's counters, and the kernel's log lines about it.
#
# In its own directory: it is damaged on purpose, and the suites that
# walk every image in test-disks/ would rightly complain about it.
# ---------------------------------------------------------------------
build_scrub() {
    local img="$WORK/btrfs-scrub.img" manifest="$WORK/btrfs-scrub.manifest"
    local data tree p
    command -v btrfs-map-logical >/dev/null || {
        echo "guest-build-images.sh: btrfs-map-logical (btrfs-progs) is not in the guest." >&2
        exit 1
    }
    rm -f "$img" "$manifest"
    truncate -s 512M "$img"
    mkfs.btrfs -f -d dup -m dup "$img" >/dev/null
    mount -o loop "$img" "$MNT"
    dd if=/dev/urandom of="$MNT/a.bin" bs=64K count=4 status=none
    echo "small" > "$MNT/small.txt"
    sync
    umount "$MNT"

    data=$(btrfs inspect-internal dump-tree -t 5 "$img" \
        | awk '/extent data disk byte/ && $5 != 0 { print $5; exit }')
    tree=$(btrfs inspect-internal dump-tree -t 5 "$img" \
        | awk '/^(leaf|node) [0-9]+ / { print $2; exit }')
    [ -n "$data" ] && [ -n "$tree" ] || {
        echo "guest-build-images: btrfs-scrub: no data extent or fs tree root found" >&2
        exit 1
    }
    # copy <logical> <mirror from 1>: the physical offset of that copy.
    copy() {
        btrfs-map-logical -l "$1" -b 4096 "$img" 2>/dev/null \
            | awk -v m="$2" '$1 == "mirror" && $2 == m { print $6; exit }'
    }
    {
        echo "# damaged: <what> <logical> <copy, from 1>"
        for spec in "data $data 2" "data $((data + 4096)) 1" "tree $tree 2"; do
            set -- $spec
            p=$(copy "$2" "$3")
            [ -n "$p" ] || {
                echo "guest-build-images: btrfs-scrub: no copy $3 of $2" >&2
                exit 1
            }
            # 64 bytes inside the sector, past a tree block's checksum
            # field: the block no longer verifies, and is still one.
            dd if=/dev/urandom of="$img" bs=1 seek=$((p + 512)) count=64 \
                conv=notrunc status=none
            echo "damaged $1 $2 $3"
        done
    } > "$manifest"

    dmesg -C 2>/dev/null || true
    mount -o loop,ro "$img" "$MNT"
    {
        echo "# btrfs scrub start -B -R -r"
        btrfs scrub start -B -R -r "$MNT" 2>&1 || true
        echo "# kernel log"
        dmesg 2>/dev/null | grep -i 'btrfs' || true
    } >> "$manifest"
    umount "$MNT"
    grep -q 'csum_errors' "$manifest" || {
        echo "guest-build-images: btrfs-scrub: the scrub printed no counters" >&2
        exit 1
    }
    mkdir -p "$OUT/scrub"
    local f
    for f in "$img" "$manifest"; do
        cp --sparse=always "$f" "$OUT/scrub/$(basename "$f").partial"
        mv -f "$OUT/scrub/$(basename "$f").partial" "$OUT/scrub/$(basename "$f")"
        rm -f "$f"
    done
    note "built scrub/btrfs-scrub"

    build_scrub_parity raid5 3
    build_scrub_parity raid6 4
}

# ---------------------------------------------------------------------
# scrub parity — RAID5 on three devices and RAID6 on four, each with a
# parity element damaged on purpose: P in one full stripe of an 8 MiB
# file, and on RAID6 also Q in the next (#301).
#
# A read never looks at parity while the data verifies, so a damaged P
# or Q is found only by a scrub that recomputes it. WHERE THE ELEMENT
# LIES IS WORKED OUT HERE, never by the reader under test: the chunk as
# `dump-tree -t chunk` prints it, and the rotation the kernel uses — data
# element i of full stripe f on stripe (i + f) mod n, P on (k + f) mod n,
# Q on (k + 1 + f) mod n, with k = n - 1 or n - 2 data elements.
#
# Then a copy of the damaged pool is mounted and scrubbed by the kernel
# with a scrub that may write, which recomputes RAID5/6 parity and
# rewrites what it finds wrong (the read-only scrub does not check
# parity). The element as the kernel rewrote it is hashed into the
# manifest, beside the addresses of the data elements it was computed
# from; the copy is discarded.
#
#   scrub/btrfs-scrub-<profile>-<n>.img   the damaged members, devid order
#   scrub/btrfs-scrub-<profile>.manifest
#     damaged <P|Q> <devid> <physical> <stripe_len>
#     data <P|Q> <devid>:<physical>...   the full stripe's data elements, in order
#     kernel <P|Q> <sha256 of the element after the kernel's scrub>
# ---------------------------------------------------------------------
build_scrub_parity() {
    local profile="$1" count="$2" parity i f
    local manifest="$WORK/btrfs-scrub-$profile.manifest"
    local loops=() imgs=() copies=()
    case "$profile" in
        raid5) parity=1 ;;
        raid6) parity=2 ;;
    esac
    for i in $(seq 1 "$count"); do
        f="$WORK/btrfs-scrub-$profile-$i.img"
        rm -f "$f"
        truncate -s 256M "$f"
        imgs+=("$f")
        loops+=("$(losetup --find --show "$f")")
    done
    scrub_parity_cleanup() {
        mountpoint -q "$MNT" && umount "$MNT" || true
        for f in "${loops[@]}"; do losetup -d "$f" 2>/dev/null || true; done
        btrfs device scan --forget >/dev/null 2>&1 || true
    }
    trap 'scrub_parity_cleanup; cleanup' EXIT

    mkfs.btrfs -f -d "$profile" -m "$profile" "${loops[@]}" >/dev/null 2>&1
    mount "${loops[0]}" "$MNT"
    dd if=/dev/urandom of="$MNT/big.bin" bs=1M count=8 status=none
    echo "small" > "$MNT/small.txt"
    sync
    umount "$MNT"

    # big.bin's data extent: the largest one the fs tree names.
    local x nr
    read -r x nr < <(btrfs inspect-internal dump-tree -t 5 "${loops[0]}" \
        | awk '/extent data disk byte/ && $5 != 0 && $7 + 0 > best + 0 { best = $7; at = $5 }
               END { print at, best }')
    # The data chunk holding it (type DATA|..., not METADATA): logical,
    # stripe_len, stripes, then each stripe as devid:offset in stripe order.
    local chunk
    chunk=$(btrfs inspect-internal dump-tree -t chunk "${loops[0]}" | awk -v x="$x" '
        /CHUNK_ITEM/ { logical = $6; sub(/\)$/, "", logical); next }
        $1 == "length" { len = $2; stripe_len = $6; type = $8; next }
        $1 == "stripe" && $3 == "devid" {
            dev[$2] = $4; off[$2] = $6; n = $2 + 1
            if (type ~ /^DATA/ && x + 0 >= logical + 0 && x + 0 < logical + len) {
                line = logical " " stripe_len " " n
                for (j = 0; j < n; j++) line = line " " dev[j] ":" off[j]
                found = line
            }
        }
        END { print found }')
    local logical stripe_len n rest
    local -a stripes
    read -r logical stripe_len n rest <<< "$chunk"
    read -r -a stripes <<< "$rest"
    [ -n "$x" ] && [ -n "$logical" ] && [ "$n" = "$count" ] && [ "${#stripes[@]}" = "$count" ] || {
        echo "guest-build-images: btrfs-scrub-$profile: no $count-stripe data chunk holds the file (extent '$x', chunk '$chunk')" >&2
        exit 1
    }
    local k=$((n - parity))
    local fss=$((k * stripe_len))
    local first=$(( (x - logical + fss - 1) / fss ))
    [ $(( (first + 2) * fss )) -le $(( x - logical + nr )) ] || {
        echo "guest-build-images: btrfs-scrub-$profile: the file's extent holds fewer than two whole full stripes" >&2
        exit 1
    }

    # element <stripe index> <full stripe>: devid:physical.
    element() {
        echo "${stripes[$1]%%:*}:$(( ${stripes[$1]#*:} + $2 * stripe_len ))"
    }
    local what fs at line
    local specs=("P $first")
    [ "$parity" = 2 ] && specs+=("Q $((first + 1))")
    : > "$manifest"
    for spec in "${specs[@]}"; do
        read -r what fs <<< "$spec"
        case "$what" in
            P) at=$(element $(( (k + fs) % n )) "$fs") ;;
            Q) at=$(element $(( (k + 1 + fs) % n )) "$fs") ;;
        esac
        echo "damaged $what ${at%%:*} ${at#*:} $stripe_len" >> "$manifest"
        line="data $what"
        for i in $(seq 0 $((k - 1))); do
            line="$line $(element $(( (i + fs) % n )) "$fs")"
        done
        echo "$line" >> "$manifest"
    done

    for f in "${loops[@]}"; do losetup -d "$f"; done
    loops=()
    btrfs device scan --forget >/dev/null 2>&1 || true

    # 64 bytes inside the element's first sector.
    local devid physical
    while read -r _ what devid physical _; do
        dd if=/dev/urandom of="${imgs[$((devid - 1))]}" bs=1 seek=$((physical + 512)) count=64 \
            conv=notrunc status=none
    done < <(grep '^damaged ' "$manifest")

    # The kernel's scrub, on a copy: it rewrites the parity it finds wrong.
    for f in "${imgs[@]}"; do
        cp --sparse=always "$f" "$f.kernel"
        copies+=("$f.kernel")
        loops+=("$(losetup --find --show "$f.kernel")")
    done
    btrfs device scan "${loops[@]}" >/dev/null
    mount "${loops[0]}" "$MNT"
    {
        echo "# btrfs scrub start -B -R"
        btrfs scrub start -B -R "$MNT" 2>&1 | sed 's/^/# /' || true
    } >> "$manifest"
    umount "$MNT"
    for f in "${loops[@]}"; do losetup -d "$f"; done
    loops=()
    btrfs device scan --forget >/dev/null 2>&1 || true
    trap cleanup EXIT

    local before after
    while read -r _ what devid physical len; do
        before=$(dd if="${imgs[$((devid - 1))]}" bs=4096 skip=$((physical / 4096)) \
            count=$((len / 4096)) status=none | sha256sum | cut -d' ' -f1)
        after=$(dd if="${copies[$((devid - 1))]}" bs=4096 skip=$((physical / 4096)) \
            count=$((len / 4096)) status=none | sha256sum | cut -d' ' -f1)
        [ "$before" != "$after" ] || {
            echo "guest-build-images: btrfs-scrub-$profile: the kernel's scrub left the damaged $what element at device $devid, $physical as it was" >&2
            exit 1
        }
        echo "kernel $what $after" >> "$manifest"
    done < <(grep '^damaged ' "$manifest")
    rm -f "${copies[@]}"

    mkdir -p "$OUT/scrub"
    for f in "${imgs[@]}" "$manifest"; do
        cp --sparse=always "$f" "$OUT/scrub/$(basename "$f").partial"
        mv -f "$OUT/scrub/$(basename "$f").partial" "$OUT/scrub/$(basename "$f")"
        rm -f "$f"
    done
    note "built scrub/btrfs-scrub-$profile-1..$count (${profile^^} with parity damaged on purpose)"
}

# ---------------------------------------------------------------------
# seed — a seed filesystem and the filesystem sprouted from it (#270).
#
# `btrfstune -S 1` marks a filesystem as a seed: it mounts read-only
# from then on, and `btrfs device add` grows a new, writable filesystem
# on top of it, with its own fsid, whose chunks live partly on the seed
# device and partly on the new one. The seed device's own superblock
# still names the seed's fsid, which is what a reader that pairs devices
# by fsid trips over.
#
#   seed/btrfs-seed.img      the seed, by itself a complete filesystem
#   seed/btrfs-sprout.img    the device added to it
#   seed/btrfs-seed.manifest    every file of the seed, as the kernel read it
#   seed/btrfs-sprout.manifest  every file of the sprouted filesystem: the
#                               seed's (one of them rewritten in the
#                               sprout) and the new ones
#
# In its own directory: the seed is refused for writing on purpose, and
# the sprout is one half of a two-device filesystem.
# ---------------------------------------------------------------------
manifest_of() {
    ( cd "$1" && find . -mindepth 1 -type f | sort | while read -r p; do
        printf '%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" \
            "$(sha256sum "$p" | cut -d' ' -f1)"
      done )
}

build_seed() {
    local seed="$WORK/btrfs-seed.img" sprout="$WORK/btrfs-sprout.img"
    local sm="$WORK/btrfs-seed.manifest" pm="$WORK/btrfs-sprout.manifest"
    local ls lp i f
    rm -f "$seed" "$sprout" "$sm" "$pm"
    truncate -s 256M "$seed"
    truncate -s 256M "$sprout"
    mkfs.btrfs -f "$seed" >/dev/null
    mount -o loop "$seed" "$MNT"
    mkdir -p "$MNT/old"
    for i in 1 2 3; do
        echo "seed file $i" > "$MNT/old/file-$i.txt"
    done
    dd if=/dev/urandom of="$MNT/old/big.bin" bs=1M count=2 status=none
    sync
    manifest_of "$MNT" > "$sm"
    umount "$MNT"
    btrfstune -S 1 "$seed"

    ls=$(losetup --find --show "$seed")
    lp=$(losetup --find --show "$sprout")
    seed_cleanup() {
        mountpoint -q "$MNT" && umount "$MNT" || true
        losetup -d "$ls" 2>/dev/null || true
        losetup -d "$lp" 2>/dev/null || true
        btrfs device scan --forget >/dev/null 2>&1 || true
    }
    trap 'seed_cleanup; cleanup' EXIT
    # A seed mounts read-only; adding a device to it sprouts the new
    # filesystem, and the remount makes that one writable.
    mount "$ls" "$MNT"
    btrfs device add -f "$lp" "$MNT" >/dev/null
    mount -o remount,rw "$MNT"
    mkdir -p "$MNT/new"
    for i in 1 2; do
        echo "sprout file $i" > "$MNT/new/file-$i.txt"
    done
    dd if=/dev/urandom of="$MNT/new/big.bin" bs=1M count=2 status=none
    # A seed file rewritten: the new copy is written into the sprout's
    # chunks, and the old one stays on the seed.
    echo "rewritten in the sprout" > "$MNT/old/file-1.txt"
    sync
    manifest_of "$MNT" > "$pm"
    umount "$MNT"
    losetup -d "$ls"
    losetup -d "$lp"
    btrfs device scan --forget >/dev/null 2>&1 || true
    trap cleanup EXIT

    btrfs inspect-internal dump-super "$seed" | grep -q 'SEEDING' || {
        echo "guest-build-images: btrfs-seed does not carry the SEEDING flag" >&2
        exit 1
    }
    [ "$(btrfs inspect-internal dump-super "$sprout" | awk '$1 == "num_devices" { print $2 }')" = 2 ] || {
        echo "guest-build-images: btrfs-sprout does not span two devices" >&2
        exit 1
    }
    mkdir -p "$OUT/seed"
    for f in "$seed" "$sprout" "$sm" "$pm"; do
        cp --sparse=always "$f" "$OUT/seed/$(basename "$f").partial"
        mv -f "$OUT/seed/$(basename "$f").partial" "$OUT/seed/$(basename "$f")"
        rm -f "$f"
    done
    note "built seed/btrfs-seed and seed/btrfs-sprout (a seed and the filesystem sprouted from it)"
}

# ---------------------------------------------------------------------
# squota — simple quotas (`-O squota`), with content (#270). The
# manifest is every file's size and SHA-256, then a comment line counting
# the owner references in the extent tree.
#
# MADE BY MKFS, NOT FILLED BY THE KERNEL. Simple quotas arrived in Linux
# and btrfs-progs 6.7; the guest runs Debian 12's 6.1 kernel, which
# refuses to mount such a volume, and Debian's btrfs-progs 6.2, which
# does not know the feature (CI run 37610992432). So the static
# btrfs-progs scripts/vm-setup.sh installs formats it with `--rootdir`,
# copying a directory tree in as it builds the filesystem, and its own
# `btrfs check` and `dump-super` are the oracles that it is sound and
# carries the feature. Its extents carry no owner references; a volume
# with them needs a guest kernel of 6.7 or later.
#
# In its own directory: it is refused for writing on purpose, and the
# suites that walk every image in test-disks/ try to write each one.
# ---------------------------------------------------------------------
build_squota() {
    local img="$WORK/btrfs-squota.img" manifest="$WORK/btrfs-squota.manifest"
    local src="$WORK/squota-root" i
    rm -rf "$img" "$manifest" "$src"
    mkdir -p "$src/dir"
    for i in 1 2 3; do
        echo "squota file $i" > "$src/dir/file-$i.txt"
    done
    dd if=/dev/urandom of="$src/big.bin" bs=1M count=4 status=none
    dd if=/dev/urandom of="$src/dir/middle.bin" bs=64K count=3 status=none
    ( cd "$src" && find . -mindepth 1 -type f | sort | while read -r p; do
        printf '%s\t%s\t%s\n' "${p#.}" "$(stat -c%s "$p")" \
            "$(sha256sum "$p" | cut -d' ' -f1)"
      done ) > "$manifest"
    truncate -s 512M "$img"
    "$STATIC_PROGS/mkfs.btrfs" -f -O squota --rootdir "$src" "$img" >/dev/null
    rm -rf "$src"
    "$STATIC_PROGS/btrfs" inspect-internal dump-super "$img" | grep -q 'SIMPLE_QUOTA' || {
        echo "guest-build-images: btrfs-squota does not carry SIMPLE_QUOTA" >&2
        exit 1
    }
    # Recorded, not required: mkfs adds no owner reference to the extents
    # it copies in (CI run 37619450394), as the kernel adds none to an
    # extent that predates simple quotas being enabled. Only a kernel
    # that knows the feature writes them, and this guest's does not.
    printf '# owner references: %s\n' "$("$STATIC_PROGS/btrfs" inspect-internal \
        dump-tree -t extent "$img" | grep -c 'EXTENT_OWNER_REF' || true)" >> "$manifest"
    "$STATIC_PROGS/btrfs" check --readonly "$img" >/dev/null 2>&1 || {
        echo "guest-build-images: btrfs check does not find btrfs-squota clean" >&2
        "$STATIC_PROGS/btrfs" check --readonly "$img" >&2 || true
        exit 1
    }
    mkdir -p "$OUT/squota"
    local f
    for f in "$img" "$manifest"; do
        cp --sparse=always "$f" "$OUT/squota/$(basename "$f").partial"
        mv -f "$OUT/squota/$(basename "$f").partial" "$OUT/squota/$(basename "$f")"
        rm -f "$f"
    done
    note "built squota/btrfs-squota"
}

# ---------------------------------------------------------------------

echo "[guest] $(btrfs --version 2>&1 | head -1) on $(uname -srm)"
echo "[guest] static: $("$STATIC_PROGS/mkfs.btrfs" --version 2>&1 | sed -n 1p)"
for target in "$@"; do
    started=$(date +%s)
    "build_$target"
    note "$target: $(( $(date +%s) - started ))s"
done
