#!/usr/bin/env bash
#
# vm-build-split-fixtures.sh — run build-split-fixtures.sh in the VM.
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guest="$("$REPO/scripts/vm.sh" put "$REPO/scripts/build-split-fixtures.sh")"
"$REPO/scripts/vm.sh" run "BTRFS_FIXTURE_DIR=/share BTRFS_SPLIT_VARY=${BTRFS_SPLIT_VARY:-0} BTRFS_SPLIT_SUFFIX=${BTRFS_SPLIT_SUFFIX:-} bash '$guest'"
rm -f "$REPO/.vm-share/build-split-fixtures.sh"
