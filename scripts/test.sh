#!/usr/bin/env bash
# test.sh [cargo test args...]  run the suite in an owned scratch directory
# test.sh --print-temp-dir      print that directory and exit
#
# SCRATCH LIVES IN THE REPOSITORY, always: tmp/ (gitignored), and never
# the system temporary directory or a runner-supplied one. The oracle
# tools run inside the fs-linux-test-harness VM, which sees this
# repository at the path the host knows it by and nothing else of the
# host — so an image anywhere else is a path the tool asked to read it
# cannot open. The same rule is written in Rust in
# tests/support/src/lib.rs (select_temp_dir), and
# tests/test_contract.rs checks that no test writes anywhere else.
#
# FS_BTRFS_TEST_TMPDIR supplies an exact directory instead; it must be
# inside the repository, and it is the caller's to delete.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The release profile is for shipped artifacts. Unit tests use Cargo's test
# profile implicitly; the other tiers name it explicitly.
previous_arg=""
for arg in "$@"; do
    [[ "$arg" == -- ]] && break
    if [[ ( "$previous_arg" == --profile && "$arg" == release ) ||
          "$arg" == --release || "$arg" == -r || "$arg" == --profile=release ]]; then
        echo "test.sh: release optimisations are for shipped artifacts; use --profile test" >&2
        exit 2
    fi
    previous_arg="$arg"
done
RUN_DIR=""

cleanup() {
    if [[ -n "$RUN_DIR" && -d "$RUN_DIR" ]]; then
        find "$RUN_DIR" -depth -mindepth 1 -delete
        rmdir "$RUN_DIR"
    fi
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ -n "${FS_BTRFS_TEST_TMPDIR:-}" ]]; then
    case "$FS_BTRFS_TEST_TMPDIR" in
        "$REPO"/*) ;;
        *)
            echo "test.sh: FS_BTRFS_TEST_TMPDIR is $FS_BTRFS_TEST_TMPDIR, which is outside" >&2
            echo "         $REPO. The oracle tools run in the harness VM, which sees this" >&2
            echo "         repository and nothing else of the host." >&2
            exit 1
            ;;
    esac
    # An exact caller-supplied directory is not ours to delete.
    mkdir -p "$FS_BTRFS_TEST_TMPDIR"
else
    mkdir -p "$REPO/tmp"
    RUN_DIR="$(mktemp -d "$REPO/tmp/fs-btrfs-tests.XXXXXX")"
    export FS_BTRFS_TEST_TMPDIR="$RUN_DIR"
fi
export TMPDIR="$FS_BTRFS_TEST_TMPDIR"

if [[ "${1:-}" == "--print-temp-dir" ]]; then
    printf '%s\n' "$FS_BTRFS_TEST_TMPDIR"
    exit 0
fi

# The harness owns the shared guest. Run cargo in a session so the
# harness knows which tests are using it; ending a session leaves the
# guest up for the next run, and `vm:down` stops it when nobody remains.
# `vm.sh session` runs cargo directly inside the guest and on hosts
# without VM tools, so the same wrapper serves every test tier.
HARNESS="$REPO/../fs-linux-test-harness"
# `--features cli` builds the command-line tools (the `rust-fs-btrfs`
# target requires it), so every tier reaches their tests. The library a
# consumer links is built without it, and gains nothing from it.
if [[ -x "$HARNESS/scripts/vm.sh" ]]; then
    export FLTH_CONFIG="$REPO/fs-linux-test-harness.toml"
    "$HARNESS/scripts/vm.sh" session cargo test --features cli "$@"
else
    cargo test --features cli "$@"
fi
