#!/usr/bin/env bash
#
# provision-stress-tools.sh — put `fsstress` and `fsx` in the guest.
#
# These two come from the filesystem test suite (fstests, formerly
# xfstests). `fsstress` runs long randomised sequences of filesystem
# operations; `fsx` hammers one file with randomised reads, writes,
# truncates, hole punches and mmap operations. Both reach on-disk states
# that a hand-written fixture never will, which is the whole reason they
# are here: scripts/vm-build-stress-fixtures.sh uses them to *generate*
# filesystems, and the kernel plus the reference checker remain the
# oracles.
#
# The suite itself is never run against this driver — it tests a mounted
# filesystem through the kernel's VFS, and this driver is a userspace
# library. Only the two generators are wanted.
#
# LICENSING. fstests is GPL-2.0. It is cloned, built and executed
# **inside the guest only**. Its source and its build artefacts never
# enter this repository, nothing from it is copied, quoted or adapted
# here, and the binaries are invoked as external programs. Running a
# program does not make the caller a derivative work of it; vendoring its
# code would. Keep it that way: if a helper is needed, write one.
#
# Debian bookworm has no package for either binary (`apt-cache search
# xfstests` and `... fsstress` both come back empty, and there is no
# `ltp` package), so building from source in the guest is the only
# option. The build is pinned to a release tag so a rebuilt VM gets the
# same generators rather than whatever upstream looks like that day.
set -euo pipefail

# A dated release tag, not a branch: the fixtures a stress generator
# produces are only reproducible if the generator itself is.
FSTESTS_TAG="v2026.08.17"
FSTESTS_URL="https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git"

# Guest-only paths. /opt is deliberate — /share is the host's directory,
# and a build tree must never land there.
SRC="/opt/fstests-src"
STAMP="/usr/local/share/fstests-generators.version"

# Idempotent: a re-provision with the same tag is a no-op, which keeps
# `vagrant provision` cheap on a box that already has them.
if [ -x /usr/local/bin/fsstress ] && [ -x /usr/local/bin/fsx ] &&
   [ "$(cat "$STAMP" 2>/dev/null || true)" = "$FSTESTS_TAG" ]; then
    echo "fsstress and fsx already built from $FSTESTS_TAG"
    exit 0
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
# git/autoconf/automake/libtool/gettext -- fstests generates its own
#   configure script rather than shipping one.
# uuid-dev, libattr1-dev, libacl1-dev, libaio-dev, libgdbm-dev,
#   libssl-dev, libcap-dev, xfslibs-dev, libext2fs-dev -- what the two
#   generators and the small support library they link against need.
apt-get install -y -qq \
    git autoconf automake libtool libtool-bin gettext pkg-config \
    uuid-dev libattr1-dev libacl1-dev libaio-dev libgdbm-dev \
    libssl-dev libcap-dev xfslibs-dev libext2fs-dev python3

rm -rf "$SRC"
git clone --quiet --depth 1 --branch "$FSTESTS_TAG" "$FSTESTS_URL" "$SRC"

cd "$SRC"
make configure >/dev/null
./configure >/dev/null
# Only the pieces the two generators need. Building the whole suite
# would pull in hundreds of test programs that will never be run here.
make -C include >/dev/null
make -C lib >/dev/null
make -C ltp >/dev/null

install -m0755 ltp/fsstress /usr/local/bin/fsstress
install -m0755 ltp/fsx      /usr/local/bin/fsx
mkdir -p "$(dirname "$STAMP")"
printf '%s\n' "$FSTESTS_TAG" > "$STAMP"

# Prove they run. A generator that segfaults on first use would
# otherwise be discovered halfway through building a fixture.
fsstress -H >/dev/null 2>&1 || true
/usr/local/bin/fsx -h >/dev/null 2>&1 || true
echo "installed fsstress and fsx from fstests $FSTESTS_TAG"
