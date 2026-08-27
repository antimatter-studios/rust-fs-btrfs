#!/usr/bin/env bash
#
# vm-build-commit-fixtures.sh — run build-commit-fixtures.sh in the
# oracle VM.
#
# The fixture needs mkfs.btrfs and the ability to mount, which a macOS
# host has neither of. Everything about what the fixture IS lives in that
# script, so CI runs the same file directly.
#
#   ./scripts/vm-build-commit-fixtures.sh
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guest_path="$("$REPO/scripts/vm.sh" put "$REPO/scripts/build-commit-fixtures.sh")"
"$REPO/scripts/vm.sh" run "BTRFS_FIXTURE_DIR=/share bash '$guest_path'"
rm -f "$REPO/.vm-share/build-commit-fixtures.sh"
