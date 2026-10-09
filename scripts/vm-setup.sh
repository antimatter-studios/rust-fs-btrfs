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
#   acl          setfacl/getfacl: the ACL fixture sets its ACLs, finds the
#                largest one the node size admits, and takes the entry
#                counts it records, through them
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
# AND WHAT A RUST BUILD NEEDS FROM THE DISTRIBUTION (curl, gcc, libc6-dev,
# pkg-config), for `chore test:vm`: the whole suite compiled and run in
# here, which is how a macOS host runs a Linux test suite at all. NOT THE
# TOOLCHAIN ITSELF: scripts/guest-suite.sh installs that through
# `../rust-fs-core/scripts/guest-rust-toolchain.sh`, rust-fs-core's one copy of the
# install, which every driver runs and which recovers from an install a
# reaper or a deadline interrupted. It cannot run from here: the harness
# ships this one file into the guest, before `test:vm` has staged the core
# sibling on the share (rust-fs-core#190).
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

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

# A SECOND, NEWER BTRFS-PROGS, FOR THE FEATURES DEBIAN'S CANNOT MAKE.
# Debian 12's mkfs.btrfs 6.2 answers `-O block-group-tree` and `-O squota`
# with "unrecognized filesystem feature" (CI runs 37609259686 and
# 37610992432), so the fixtures for those features (#270) are formatted by
# btrfs-progs' own static release build instead. It goes in a directory
# of its own and is called by path, never put on PATH: every other
# fixture and every oracle keeps the distribution's version, so no
# existing answer moves. The checksum pins the exact binary.
static_progs_version=7.1
static_progs_dir=/opt/btrfs-progs-static
case "$(uname -m)" in
    x86_64)
        static_progs_asset=btrfs.box.static
        static_progs_sha256=ed9a8815d12e40d2bf413d1259133a6727106715a2e5d8095179439b5d3f1467
        ;;
    aarch64)
        static_progs_asset=btrfs.box.static-arm
        static_progs_sha256=42c59468de77ad5fac2b9eb378ee9201ad501d8fcbc588807ce6ff123dfbdac6
        ;;
    *)
        echo "vm-setup: btrfs-progs publishes no static build for $(uname -m)" >&2
        exit 1
        ;;
esac
if ! echo "$static_progs_sha256  $static_progs_dir/btrfs.box" | sha256sum -c --status 2>/dev/null; then
    mkdir -p "$static_progs_dir"
    curl -fsSL --retry 5 --retry-all-errors -o "$static_progs_dir/btrfs.box.partial" \
        "https://github.com/kdave/btrfs-progs/releases/download/v$static_progs_version/$static_progs_asset"
    echo "$static_progs_sha256  $static_progs_dir/btrfs.box.partial" | sha256sum -c --status || {
        echo "vm-setup: btrfs-progs v$static_progs_version $static_progs_asset does not match its pinned checksum" >&2
        exit 1
    }
    chmod 755 "$static_progs_dir/btrfs.box.partial"
    mv -f "$static_progs_dir/btrfs.box.partial" "$static_progs_dir/btrfs.box"
fi
# The box is one binary that acts as whichever tool it is invoked as.
for tool in btrfs mkfs.btrfs btrfstune; do
    ln -sfn btrfs.box "$static_progs_dir/$tool"
done
"$static_progs_dir/mkfs.btrfs" --version 2>&1 | sed -n 1p

echo "vm-setup: btrfs-progs and the oracle tools are installed in the guest"
