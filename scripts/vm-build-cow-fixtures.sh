#!/usr/bin/env bash
#
# vm-build-cow-fixtures.sh — run build-cow-fixtures.sh in the oracle VM.
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guest="$("$REPO/scripts/vm.sh" put "$REPO/scripts/build-cow-fixtures.sh")"
"$REPO/scripts/vm.sh" run "BTRFS_FIXTURE_DIR=/share bash '$guest'"
rm -f "$REPO/.vm-share/build-cow-fixtures.sh"
