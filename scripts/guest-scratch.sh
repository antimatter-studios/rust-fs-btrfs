#!/usr/bin/env bash
# Run a guest command with repository scratch paths backed by the guest disk.
set -euo pipefail

[[ "${FLTH_GUEST:-}" == 1 && $# -ge 2 ]] || {
    echo "guest-scratch.sh needs a harness guest, a repository, and a command." >&2
    exit 2
}
repo="$1"
shift
target="$repo/tmp"
mkdir -p "$target"
if mountpoint -q "$target"; then
    echo "guest-scratch.sh: $target already holds a mount." >&2
    exit 1
fi
scratch="$(mktemp -d /var/cache/fs-btrfs-scratch.XXXXXX)"
mounted=0
cleanup() {
    result=$?
    trap - EXIT
    if [[ "$mounted" == 1 ]] && ! umount "$target"; then
        echo "guest-scratch.sh: TEARDOWN FAILED: $target is still mounted from $scratch." >&2
        sync || true
        exit 1
    fi
    # Tests retain oracle evidence in tmp/logs outside their run directory.
    # Merge it back into the host-visible logs before releasing scratch.
    if [[ -d "$scratch/logs" ]]; then
        if ! { mkdir -p "$target/logs" && cp -R "$scratch/logs/." "$target/logs/"; }; then
            echo "guest-scratch.sh: TEARDOWN FAILED: oracle logs remain at $scratch/logs." >&2
            sync || true
            exit 1
        fi
    fi
    # This freshly allocated directory is owned by this run. Tests may
    # leave artifacts outside test.sh's own run directory; release them
    # only after the bind mount has been removed.
    rm -rf -- "$scratch" || result=1
    # The harness may stop QEMU immediately after this command returns.
    # Persist the build cache and cleanup before that stop.
    if ! sync; then
        echo "guest-scratch.sh: TEARDOWN FAILED: guest writes could not be synced." >&2
        result=1
    fi
    exit "$result"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
mount --bind "$scratch" "$target"
mounted=1
"$@"
