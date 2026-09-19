#!/usr/bin/env bash
#
# vm-setup.sh — the fs-linux-test-harness [setup] script. Runs as root
# INSIDE the VM, re-applied by the harness whenever this file changes.
#
# THE GUEST IS WHERE THE ORACLE TOOLS LIVE. Not the host: btrfs-progs on
# a workstation is whatever that machine has — a Homebrew formula on a
# Mac, a distribution build on Linux, a different version per developer
# — and on a Mac it is not even the platform these images are for. One
# Debian guest, one version, the same answers for everyone.
#
#   btrfs-progs  mkfs.btrfs, `btrfs check`, `btrfs inspect-internal
#                dump-super/dump-tree`, `btrfs subvolume` — the oracle
#                tools (tests/support/src/oracle.rs) and the fixture
#                builder's formatter
#   attr         setfattr/getfattr: the extended-attribute fixture both
#                sets attributes with and takes its reference answer
#                from. Without it that fixture builds a filesystem with
#                no attributes and a manifest saying so
#   e2fsprogs    chattr/lsattr, which is how a directory is marked
#                NODATACOW — an ext2 tool that btrfs honours
#   python3      the compressible payloads the compression fixtures need
#   util-linux   losetup and mount: the loop mounts, which happen here
#                and nowhere else
#
# THE EXOTIC CHECKSUM MODULES. xxhash, sha256 and blake2 live in
# separate crypto modules, and a mount of such an image is a bad place
# to discover they are missing. They are loaded here so a fixture build
# fails with the reason rather than with "invalid argument".
#
# AND A RUST TOOLCHAIN, for `chore test:vm` — the whole suite compiled
# and run in here, which is how a macOS host runs a Linux test suite at
# all. It is pinned to the repository's rust-toolchain.toml, installed
# under /var/lib (the VM's own disk, which outlives a `vm:down`), and
# the build directory lives there too so the second run is incremental.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

REPO=/repo
RUST_ROOT=/var/lib/fs-btrfs-rust
export RUSTUP_HOME="$RUST_ROOT/rustup"
export CARGO_HOME="$RUST_ROOT/cargo"

apt-get update -qq
apt-get install -y -qq \
    btrfs-progs attr acl e2fsprogs python3 util-linux \
    curl gcc libc6-dev pkg-config >/dev/null

modprobe loop
modprobe btrfs
# Not fatal on their own: the fixture that needs one fails by name when
# mkfs.btrfs refuses the algorithm, which is a better message than a
# setup script that stops the whole VM coming up.
for module in xxhash_generic sha256_generic blake2b_generic; do
    modprobe "$module" || echo "vm-setup: note: $module unavailable"
done

# sed, not head: head exits after one line, the tool gets SIGPIPE writing
# its second, and pipefail turns that into a failed setup.
btrfs --version 2>&1 | sed -n 1p
mkfs.btrfs --version 2>&1 | sed -n 1p

# 5.16 is the first btrfs-progs whose `btrfs check` knows the block-group
# tree and whose `inspect-internal dump-tree` prints `extent compression`
# for every algorithm the compression fixtures assert on. Debian 12 ships
# 6.2.
version="$(btrfs --version 2>&1 | sed -n 's/^btrfs-progs v\([0-9][0-9.]*\).*/\1/p' | head -1)"
if [ -z "$version" ] ||
    [ "$(printf '%s\n%s\n' 5.16 "$version" | sort -V | head -1)" != 5.16 ]; then
    echo "vm-setup: btrfs-progs ${version:-of unknown version} is older than 5.16" >&2
    exit 1
fi

# The toolchain the repository pins, and only that one: a guest that
# silently built with a different compiler than CI is a guest whose
# result means nothing.
toolchain="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$REPO/rust-toolchain.toml" | head -1)"
[ -n "$toolchain" ] || { echo "vm-setup: no channel in $REPO/rust-toolchain.toml" >&2; exit 1; }

mkdir -p "$RUST_ROOT"
if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --no-modify-path --default-toolchain none >/dev/null
fi
"$CARGO_HOME/bin/rustup" toolchain install "$toolchain" \
    --component rustfmt --component clippy --profile minimal >/dev/null
"$CARGO_HOME/bin/rustup" default "$toolchain" >/dev/null
"$CARGO_HOME/bin/cargo" --version

echo "vm-setup: btrfs-progs and the pinned toolchain are installed in the guest"
