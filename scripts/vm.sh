#!/usr/bin/env bash
#
# vm.sh — drive the Debian arm64 oracle VM.
#
#   vm.sh up            boot the VM (idempotent; provisions on first run)
#   vm.sh run <cmd...>  run a command inside the VM
#   vm.sh share         print the host path of the shared directory
#   vm.sh put <file>    copy a file into the shared directory, echo guest path
#   vm.sh down          halt the VM (state is kept; next `up` is fast)
#   vm.sh destroy       delete the VM entirely
#
# The VM is the real-Linux oracle: mkfs.btrfs, the in-kernel Btrfs driver
# and `btrfs check` are Linux-only, and validating this driver against
# anything less than a real kernel would just be marking our own homework.
#
# The VM is kept running between invocations on purpose. Booting is the
# slow part; an iterate-and-check loop should pay it once.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VAGRANT_DIR="$REPO/tests/vagrant/debian"
SHARE="$REPO/.vm-share"

mkdir -p "$SHARE"

# Check the host has what the VM needs before trying to boot it.
#
# Without this the first symptom of a missing tool is a Vagrant error
# that names a plugin or, worse, a guest that imports and then never
# boots. The repository knows what it requires; it should say so.
require_host_tools() {
    local checker="$REPO/scripts/install-host-tools.sh"
    [ -x "$checker" ] || return 0
    if ! "$checker" --quiet; then
        echo
        echo "The VM cannot start until these are installed." >&2
        exit 1
    fi
}

vm_up() {
    # `vagrant status` is authoritative but slow-ish; only boot when the
    # machine is not already running.
    require_host_tools
    if ! (cd "$VAGRANT_DIR" && vagrant status --machine-readable 2>/dev/null \
            | grep -q ',state,running'); then
        echo "[vm] booting Debian arm64 oracle (first run provisions, ~2 min)..." >&2
        (cd "$VAGRANT_DIR" && vagrant up)
    fi
}

case "${1:-}" in
    up)
        vm_up
        ;;
    run)
        shift
        vm_up
        # `vagrant ssh -c` mangles quoting for complex commands; feed the
        # command on stdin instead so the guest shell sees it verbatim.
        printf '%s\n' "$*" | (cd "$VAGRANT_DIR" && vagrant ssh -- -T 'sudo bash -s')
        ;;
    share)
        echo "$SHARE"
        ;;
    put)
        [ $# -eq 2 ] || { echo "usage: vm.sh put <file>" >&2; exit 2; }
        cp "$2" "$SHARE/"
        echo "/share/$(basename "$2")"
        ;;
    down)
        (cd "$VAGRANT_DIR" && vagrant halt)
        ;;
    destroy)
        (cd "$VAGRANT_DIR" && vagrant destroy -f)
        ;;
    *)
        sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
esac
