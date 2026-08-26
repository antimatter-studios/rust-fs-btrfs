#!/usr/bin/env bash
#
# install-host-tools.sh — install what the oracle VM needs on this machine.
#
# Everything this project runs happens inside the VM. Four things cannot,
# because they are what starts the VM in the first place: a hypervisor, a
# filesystem-sharing daemon, and the two Vagrant plugins that drive them.
#
# They were previously a comment in the Vagrantfile, which meant a new
# machine found out what was missing by hitting an error. This script is
# the repository saying what it needs, so the answer travels with the
# code rather than living in whoever set it up last.
#
#   ./scripts/install-host-tools.sh          install anything missing
#   ./scripts/install-host-tools.sh --check  report only, change nothing
#   ./scripts/install-host-tools.sh --quiet  as --check, silent when all present
#
# `--quiet` is what `vm.sh` calls before booting, so a missing tool is
# reported with the command that fixes it rather than as a failure to
# start. It says nothing when there is nothing to say: the check runs
# before every `vm.sh run`, and a reassurance printed a hundred times a
# test run is just noise to scroll past.
set -uo pipefail

CHECK_ONLY=0
QUIET=0
case "${1:-}" in
    --check) CHECK_ONLY=1 ;;
    --quiet) CHECK_ONLY=1; QUIET=1 ;;
esac

# The tap is ours: upstream qemu and virtiofsd on Homebrew do not carry
# the patches the VM depends on.
BREW_TAP="antimatter-studios/tap"
BREW_FORMULAE=(qemu virtiofsd)
VAGRANT_PLUGINS=(vagrant-qemu-christhomas vagrant-notify-forwarder-christhomas)

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }

missing=0
REPORT=()

# Describe a missing tool the way it is most useful: name it, say what it
# is for, and give the exact command. A reader should never have to
# search for the fix.
#
# Collected rather than printed, because whether any of this is worth
# showing is only known once every check has run — and in `--quiet` mode,
# all-present means show nothing at all.
report_missing() {
    local what="$1" why="$2" how="$3"
    REPORT+=("$(red "  missing: $what")")
    REPORT+=("$(printf '           %s' "$why")")
    REPORT+=("$(printf '           install: %s' "$how")")
    missing=$((missing + 1))
}

flush_report() {
    [ ${#REPORT[@]} -eq 0 ] && return 0
    printf '%s\n' "${REPORT[@]}"
}

require_macos() {
    if [ "$(uname -s)" != "Darwin" ]; then
        bold "This script targets macOS."
        echo "On Linux the VM is unnecessary — the tools it provides run natively."
        echo "Install btrfs-progs with your package manager and run the"
        echo "tests directly."
        exit 0
    fi
}

check_brew() {
    if ! command -v brew >/dev/null 2>&1; then
        report_missing "homebrew" \
            "needed to install the hypervisor and the sharing daemon" \
            "see https://brew.sh"
        return 1
    fi
    return 0
}

check_vagrant() {
    if ! command -v vagrant >/dev/null 2>&1; then
        report_missing "vagrant" \
            "drives the VM this project's tests run inside" \
            "brew install --cask vagrant"
        return 1
    fi
    return 0
}

check_formula() {
    local f="$1"
    # `brew list` rather than `command -v`: virtiofsd is not on PATH, and
    # a system qemu would satisfy `command -v` while lacking the patches
    # the VM needs.
    if ! brew list --formula "$f" >/dev/null 2>&1; then
        report_missing "$f" \
            "$(formula_purpose "$f")" \
            "brew install $BREW_TAP/$f"
        return 1
    fi
    return 0
}

formula_purpose() {
    case "$1" in
        qemu)      echo "the hypervisor; runs the arm64 guest under HVF, so there is no emulation penalty" ;;
        virtiofsd) echo "shares .vm-share/ into the guest, which is how fixtures get in and out" ;;
        *)         echo "required by the VM" ;;
    esac
}

check_plugin() {
    local p="$1"
    if ! vagrant plugin list 2>/dev/null | grep -q "^$p "; then
        report_missing "$p" \
            "$(plugin_purpose "$p")" \
            "vagrant plugin install $p"
        return 1
    fi
    return 0
}

plugin_purpose() {
    case "$1" in
        vagrant-qemu-christhomas)
            echo "the QEMU provider; the upstream plugin lacks changes this VM depends on" ;;
        vagrant-notify-forwarder-christhomas)
            # Recorded because disabling it fails in a way that looks like
            # something else entirely.
            echo "must be present, and must stay enabled — with it disabled the guest imports and then never boots, silently" ;;
        *)  echo "required by the VM" ;;
    esac
}

install_formula() {
    local f="$1"
    bold "Installing $f"
    brew tap "$BREW_TAP" >/dev/null 2>&1 || true
    brew install "$BREW_TAP/$f"
}

install_plugin() {
    local p="$1"
    bold "Installing $p"
    vagrant plugin install "$p"
}

check_everything() {
    missing=0
    REPORT=()
    check_brew || true
    check_vagrant || true
    if command -v brew >/dev/null 2>&1; then
        for f in "${BREW_FORMULAE[@]}"; do check_formula "$f" || true; done
    fi
    if command -v vagrant >/dev/null 2>&1; then
        for p in "${VAGRANT_PLUGINS[@]}"; do check_plugin "$p" || true; done
    fi
}

require_macos

if [ "$CHECK_ONLY" = 1 ]; then
    check_everything

    if [ "$missing" -eq 0 ]; then
        [ "$QUIET" = 1 ] && exit 0
        bold "Host tools for the oracle VM"
        echo
        green "  all present"
        exit 0
    fi

    bold "Host tools for the oracle VM"
    echo
    flush_report
    echo
    echo "Install everything missing with:"
    echo "    ./scripts/install-host-tools.sh"
    exit 1
fi

bold "Host tools for the oracle VM"
echo

# Installing: bail on the two that cannot be installed for the user, and
# fix the rest.
check_brew || { flush_report; exit 1; }
check_vagrant || { flush_report; exit 1; }

for f in "${BREW_FORMULAE[@]}"; do
    check_formula "$f" >/dev/null 2>&1 || install_formula "$f"
done
for p in "${VAGRANT_PLUGINS[@]}"; do
    check_plugin "$p" >/dev/null 2>&1 || install_plugin "$p"
done

echo
check_everything

if [ "$missing" -eq 0 ]; then
    green "All host tools present. Next: ./scripts/vm.sh up"
else
    flush_report
    echo
    red "Some tools are still missing — see above."
    exit 1
fi
