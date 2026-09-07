#!/usr/bin/env bash
#
# vm-build-xattr-fixtures.sh — run build-xattr-fixtures.sh inside the
# oracle VM.
#
# The fixture needs mkfs.btrfs, setfattr and the ability to mount, which
# a macOS host has none of. This copies the builder into the shared
# folder and runs it there; everything about what the fixture IS lives in
# that script and not here, so CI can run the same file directly.
#
#   ./scripts/vm-build-xattr-fixtures.sh
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Only .vm-share is mounted in the guest, at /share — the repository is
# not. `vm.sh put` copies a file there and echoes its path in the guest.
guest_path="$("$REPO/scripts/vm.sh" put "$REPO/scripts/build-xattr-fixtures.sh")"

# The shared folder IS the output directory in the guest.
"$REPO/scripts/vm.sh" run "BTRFS_FIXTURE_DIR=/share bash '$guest_path'"

rm -f "$REPO/.vm-share/build-xattr-fixtures.sh"
