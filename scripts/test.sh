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

# Cargo runs inside a harness session so its guest calls belong to one run.
# The session ends that run and detaches its tree when cargo finishes or is
# interrupted. The shared guest remains available to other repositories;
# only the harness may decide when it is safe to stop it.
#
# Inside the guest, without the harness sibling, or on a host without VM
# support, no session is started here. Tests needing the guest report that
# requirement themselves.
HARNESS="$REPO/../fs-linux-test-harness"
session=()
if [[ "${FLTH_GUEST:-}" != 1 && -f "$HARNESS/scripts/vm-session.sh" ]] &&
    "$HARNESS/scripts/host-tools.sh" --quiet >/dev/null 2>&1; then
    export FLTH_CONFIG="$REPO/fs-linux-test-harness.toml"
    # Not `exec`, and cargo is not exec'd either: the trap belongs to
    # this shell, which must outlive cargo to run it.
    # shellcheck disable=SC2016  # expanded by the inner shell
    # Capture cargo's own status before the session exit trap runs.
    cargo_status="$FS_BTRFS_TEST_TMPDIR/.cargo-status.$$"
    rm -f "$cargo_status"
    session=(bash -c '. "$1" || exit 1; status_file="$2"; shift 2; "$@"; rc=$?; echo "$rc" > "$status_file"; exit "$rc"' fs-btrfs-test "$HARNESS/scripts/vm-session.sh" "$cargo_status")
fi

# `--features cli` builds the command-line tools (the `rust-fs-btrfs`
# target requires it), so every tier reaches their tests. The library a
# consumer links is built without it, and gains nothing from it.
#
# The `+` form because an empty array is an unbound variable to bash
# before 4.4, which is what macOS ships.
if [[ ${#session[@]} -eq 0 ]]; then
    cargo test --features cli "$@"
    exit
fi

rc=0
"${session[@]}" cargo test --features cli "$@" || rc=$?
[[ -f "$cargo_status" ]] || exit "$rc"
cargo_rc="$(cat "$cargo_status")"
rm -f "$cargo_status"

exit "$cargo_rc"
