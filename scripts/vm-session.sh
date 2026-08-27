#!/usr/bin/env bash
#
# vm-session.sh — bring the oracle VM down when the script that sourced
# this one finishes, however it finishes.
#
# `vm.sh run` and `vm.sh put` boot the machine if it is not already up,
# which is what makes the wrappers convenient to call. Nothing brought it
# back down. Every wrapper therefore left a QEMU process and a virtiofsd
# holding several gigabytes of RAM until somebody noticed, and "somebody
# noticed" was the actual teardown mechanism.
#
# Sourcing this installs an EXIT trap, so teardown also happens on a
# failure, on a `set -e` abort, and on Ctrl-C.
#
# # A failing teardown fails the script
#
# Even when the work succeeded. A machine that would not stop is not a
# footnote to a green run: it is the exact condition this exists to
# prevent, and reporting success while it is still running is how it went
# unnoticed for so long. `vm.sh down` already confirms the machine
# actually stopped rather than trusting `vagrant halt`'s exit status, so
# a failure here means it really is still up.
#
# When the work ALSO failed, that exit status wins — the earlier failure
# is the more informative one, and both are printed.
#
# # Keeping it up on purpose
#
#   KEEP_VM=1 ./scripts/vm-build-fixtures.sh
#
# for someone running several builds back to back, where booting each
# time is the slow part. It says so on the way out, so a machine left
# running is always something that was asked for.
#
# Usage, near the top of a wrapper:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

_vm_session_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

vm_session_end() {
    local code=$?
    # Clear the trap first: `vm.sh down` failing must not re-enter this.
    trap - EXIT

    if [ "${KEEP_VM:-0}" = "1" ]; then
        echo "[vm] KEEP_VM=1 — leaving the machine running, as asked." >&2
        exit "$code"
    fi

    if "$_vm_session_repo/scripts/vm.sh" down; then
        exit "$code"
    fi

    echo "vm: TEARDOWN FAILED — the machine is still running and is still using \
memory. Stop it with 'vagrant halt' in tests/vagrant/debian." >&2
    if [ "$code" -eq 0 ]; then
        # The work succeeded and the cleanup did not. That is a failure.
        exit 1
    fi
    echo "vm: the work had already failed with status $code." >&2
    exit "$code"
}

trap vm_session_end EXIT
