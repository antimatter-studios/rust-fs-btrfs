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
for image in "$@"; do
    loops+=("$(losetup --find --show --read-only "$image")")
done
btrfs device scan "${loops[@]}" >/dev/null
btrfs check --readonly "${loops[0]}"
