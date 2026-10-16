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
RUN_DIR=""
recovery_sessions=""
recovery_mode=""
recovery_lock=""

release_recovery() {
    if [[ -n "$recovery_sessions" && -n "$recovery_mode" ]]; then
        chmod "$recovery_mode" "$recovery_sessions" || return 1
        recovery_sessions=""
    fi
    if [[ -n "$recovery_lock" ]]; then
        rmdir "$recovery_lock" || return 1
        recovery_lock=""
    fi
}

cleanup() {
    release_recovery || echo "test.sh: TEARDOWN FAILED: session registry access could not be restored." >&2
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

# THE RUN OWNS THE VM ITS TESTS BOOT (#141). An oracle or kernel test
# brings the harness VM up from inside its own process, and that boot
# takes the slot -- ONE for the whole machine, shared by every
# repository. Left to chore's `after_all` reaper, which runs only inside
# a chore invocation of this repository, a run that reached cargo any
# other way (this script, ../rust-fs-core/scripts/tier.sh, by hand) exited 0 with the VM
# idle and the slot held, and every other repository's VM work queued
# behind it at 0% CPU until the guest's idle deadline.
#
# So cargo runs inside a harness session: vm-session.sh's EXIT trap
# brings the VM down and releases the slot when the run ends -- passed,
# failed or killed -- and its marker keeps another invocation's reaper
# off the VM while the run is using it. The release is the harness's
# own, which frees the slot only when this machine holds it, and only
# once the VM is confirmed stopped. FLTH_KEEP_VM=1 keeps the VM up for
# runs back to back, as it does for `chore fixtures`.
#
# No session where there is no VM to own: inside the guest (`chore
# test:vm`), without the harness sibling (a test that needs the VM then
# fails naming `chore siblings`), or on a host that cannot run the VM at
# all (CI's unit and aarch64 jobs), where a teardown would fail on a
# machine that could never have existed.
HARNESS="$REPO/../fs-linux-test-harness"
session=()
if [[ "${FLTH_GUEST:-}" != 1 && -f "$HARNESS/scripts/vm-session.sh" ]] &&
    "$HARNESS/scripts/host-tools.sh" --quiet >/dev/null 2>&1; then
    export FLTH_CONFIG="$REPO/fs-linux-test-harness.toml"
    # Not `exec`, and cargo is not exec'd either: the trap belongs to
    # this shell, which must outlive cargo to run it.
    # shellcheck disable=SC2016  # expanded by the inner shell
    # cargo's own status is written down before the session's EXIT trap
    # runs, so a teardown failure can be told apart from a test failure.
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

# FLTH_KEEP_VM is an explicit request, including when cargo failed.
if [[ "${FLTH_KEEP_VM:-0}" == 1 ]]; then
    exit "$rc"
fi
vm_status="$("$HARNESS/scripts/vm.sh" status 2>&1)" || true
case "$vm_status" in
    *'not running (stopped)'* | *'not running (absent)'*) exit "$cargo_rc" ;;
esac

# The v0.2.0 harness has no atomic check-and-destroy API. Reserve its
# session registry BEFORE checking ownership: session-begin must create
# a marker successfully before its caller can use the VM. Making an
# empty registry unwritable closes the check/destroy race. An already
# opened marker is visible to the scan, even before its contents arrive.
# Recovery calls serialize with each other; new sessions fail to enter
# instead of borrowing a VM while it is being destroyed. Root and ACLs
# that retain write access fail closed. Never remove another marker.
recovery_fail() {
    echo "test.sh: TEARDOWN FAILED: $*" >&2
    release_recovery || true
    [[ "$cargo_rc" -ne 0 ]] && exit "$cargo_rc"
    exit 1
}
[[ "$(id -u)" != 0 ]] || recovery_fail "cannot reserve session ownership as root"
machine="$("$HARNESS/scripts/vm.sh" config | sed -n 's/^machine=//p')"
[[ -n "$machine" && -d "$machine" ]] || recovery_fail "no resolved harness machine directory"
if ! mkdir "$machine/.btrfs-teardown.lock"; then
    recovery_fail "another recovery owns the session registry"
fi
recovery_lock="$machine/.btrfs-teardown.lock"
recovery_sessions="$machine/sessions"
mkdir -p "$recovery_sessions"
recovery_mode="$(stat -c '%a' "$recovery_sessions" 2>/dev/null || stat -f '%Lp' "$recovery_sessions")"
chmod a-w "$recovery_sessions" || recovery_fail "cannot reserve the session registry"
[[ ! -w "$recovery_sessions" ]] || recovery_fail "session registry remains writable"
for marker in "$recovery_sessions"/* "$recovery_sessions"/.[!.]* "$recovery_sessions"/..?*; do
    [[ -e "$marker" || -L "$marker" ]] || continue
    [[ -f "$marker" && ! -L "$marker" ]] || recovery_fail "unrecognized session marker"
    pid=""; start=""
    IFS=$'\t' read -r pid start < "$marker" || true
    [[ "$pid" =~ ^[1-9][0-9]*$ && -n "$start" ]] || recovery_fail "incomplete session marker"
    now="$(ps -o lstart= -p "$pid" 2>/dev/null | sed 's/^ *//')" || true
    [[ -n "$now" && "$now" == "$start" ]] || recovery_fail "unverified session marker"
    echo "test.sh: the VM is left running for another live session (pid $pid)." >&2
    release_recovery || recovery_fail "cannot restore session access"
    exit "$cargo_rc"
done
[[ "$vm_status" == *' is running' ]] || recovery_fail "the VM state is unknown"
echo "test.sh: the harness session ended with status $rc (cargo: $cargo_rc); retrying the teardown." >&2
if "$HARNESS/scripts/vm.sh" session-end "$$"; then
    echo "test.sh: teardown confirmed on retry." >&2
    release_recovery || recovery_fail "cannot restore session access"
    exit "$cargo_rc"
fi
echo "test.sh: the retried halt left the VM running; destroying it." >&2
if "$HARNESS/scripts/vm.sh" destroy; then
    echo "test.sh: teardown confirmed by destroy." >&2
    release_recovery || recovery_fail "cannot restore session access"
    exit "$cargo_rc"
fi
echo "test.sh: TEARDOWN FAILED after retry and destroy -- the VM is still running." >&2
"$HARNESS/scripts/vm.sh" status >&2 || true
[[ "$cargo_rc" -ne 0 ]] && exit "$cargo_rc"
exit 1
