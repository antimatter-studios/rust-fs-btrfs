#!/usr/bin/env bash
# Run through the oracle helper in the harness guest, with the sprout first.
set -euo pipefail
loops=()
cleanup() {
    for member in "${loops[@]}"; do
        btrfs device scan --forget "$member" >/dev/null 2>&1 || true
        losetup -d "$member"
    done
}
trap cleanup EXIT
# Copies of a seed pool share a filesystem identifier, so the kernel refuses to
# scan one over a remembered record of another. A udev worker still scanning
# would put a record back, so it is waited for first, and a forget that fails
# stops here rather than reading as `btrfs check` failing (#754).
udevadm settle
btrfs device scan --forget >/dev/null
for image in "$@"; do
    loops+=("$(losetup --find --show --read-only "$image")")
done
btrfs device scan "${loops[@]}" >/dev/null
btrfs check --readonly "${loops[0]}"
