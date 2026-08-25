#!/usr/bin/env bash
#
# fixture-geometries.sh — the one list of Btrfs geometries the oracle
# fixtures are built from.
#
# Sourced by both fixture builders:
#   scripts/vm-build-fixtures.sh     (macOS developer loop, builds in the VM)
#   scripts/build-fixtures-native.sh (CI, builds on the Linux runner)
#
# Keeping the list in one file is the point: if the VM gate and the CI
# gate cover different geometries, the one a developer runs stops being
# the one that guards the branch.
#
# Each entry is "<name>:<mkfs.btrfs args>". The name becomes the fixture
# filename, so keep it filesystem-safe and stable — the oracle test
# reports failures by it.
#
# Geometries are chosen to move the fields most likely to be misread:
# node size changes every b-tree item offset, the checksum algorithm
# changes both the csum_type field and the width of every csum in the
# superblock and tree blocks, the profile flags change the chunk-tree
# layout, and mixed block groups fold data and metadata into one block
# group type.

# shellcheck disable=SC2034  # consumed by the scripts that source this
BTRFS_GEOMETRIES=(
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

# Every fixture image is this size. Large enough to clear mkfs.btrfs's
# minimum device size with room for several block groups, small enough
# that ten of them are cheap to build and to share with the guest.
BTRFS_FIXTURE_SIZE="${BTRFS_FIXTURE_SIZE:-400M}"

# ---------------------------------------------------------------------
# Populated fixtures.
#
# The geometries above are all freshly-made filesystems, and every tree
# on one of those is a single leaf. That leaves internal-node parsing,
# the KeyPtr layout and the descent loop itself exercised only by
# hand-built blocks — which this project does not count as validated.
#
# These entries are mounted and filled with enough files to push the fs
# tree above level 0 (both reach level 2). tests/fstree_oracle.rs walks
# them and fails outright if no fixture has a multi-level tree, so the
# gap cannot silently reopen.
#
# Each entry is "<name>:<mkfs.btrfs args>:<file count>:<image size>".
# ---------------------------------------------------------------------

# shellcheck disable=SC2034  # consumed by the scripts that source this
BTRFS_POPULATED=(
    "deep4k:-n 4096:20000:2G"
    "deep16k:-n 16384:60000:2G"
)

# ---------------------------------------------------------------------
# The "rich" fixture.
#
# Written through a COMPRESSING mount and holding a deliberately varied
# tree: a highly compressible file (so zstd actually engages), an
# incompressible one (so the same image still has a plain extent), a
# small file stored inline in its item, a sparse file that is nearly all
# holes, a symlink, and a nested directory.
#
# It is what proves the driver refuses a compressed extent by name while
# still reading everything else on the same filesystem. Without the
# incompressible file, a driver that simply refused every read would pass
# the compression test just as well.
# ---------------------------------------------------------------------

# shellcheck disable=SC2034  # consumed by the scripts that source this
BTRFS_RICH_NAME="rich"
# shellcheck disable=SC2034
BTRFS_RICH_SIZE="600M"
# shellcheck disable=SC2034
BTRFS_RICH_MOUNT_OPTS="compress=zstd"
