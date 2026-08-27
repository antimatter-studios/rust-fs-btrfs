#!/usr/bin/env bash
#
# vm.sh — drive the Debian arm64 oracle VM.
#
#   vm.sh up            boot the VM (idempotent; provisions on first run)
#   vm.sh run <cmd...>  run a command inside the VM
#   vm.sh share         print the host path of the shared directory
#   vm.sh put <file>    copy a file into the shared directory, echo guest path
#   vm.sh down          halt the VM (state is kept; next `up` is fast)
#   vm.sh status        say whether the VM is running (exit 0 = running)
#   vm.sh reap          stop a VM nothing cleaned up (the safety net)
#   vm.sh hold          mark the VM as deliberately left running
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
# Set when someone asked for the machine directly, so `reap` leaves it
# alone. Inside .vagrant/, which is already gitignored, and beside the
# machine state it describes.
HOLD="$VAGRANT_DIR/.vagrant/keep-running"

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
        # Retried, because the forwarded SSH port is not always free the
        # instant the previous machine stops:
        #
        #   qemu-system-aarch64: Could not set up host forwarding rule
        #
        # This became routine once every wrapper started tearing down,
        # which made stop-then-start the normal pattern. Waiting for the
        # port to look free was tried first and does not work — `lsof`
        # reports it free while qemu still cannot bind it — so the retry
        # is here, where the failure actually happens, and covers every
        # cause rather than the one that was guessed at.
        for attempt in 1 2 3; do
            if (cd "$VAGRANT_DIR" && vagrant up); then
                break
            fi
            if [ "$attempt" -eq 3 ]; then
                echo "vm: the machine would not boot after 3 attempts." >&2
                exit 1
            fi
            echo "[vm] boot failed, retrying in 5s (attempt $attempt of 3)..." >&2
            sleep 5
            # A half-started machine holds the resources the next attempt
            # needs, so it is stopped before trying again.
            (cd "$VAGRANT_DIR" && vagrant halt -f) >/dev/null 2>&1 || true
        done
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
        # Halt, then CONFIRM. `vagrant halt` reporting success is not the
        # same as the machine being down, and this runs as a `defer:` in
        # chores.yml — so its exit status is the only thing standing
        # between a leaked QEMU process and a green test run.
        #
        # A teardown that says it worked while the VM is still up is the
        # exact failure the defer was added to prevent, and it is worse
        # than one that fails loudly: nobody looks again at a green run.
        rm -f "$HOLD"
        (cd "$VAGRANT_DIR" && vagrant halt) || true
        state=$(cd "$VAGRANT_DIR" && vagrant status --machine-readable 2>/dev/null \
                | sed -n 's/.*,state,//p' | head -1)
        case "$state" in
            running)
                echo "vm: halt did not stop the machine — it is still running." >&2
                echo "    Left as it is rather than force-killed; \`vm.sh destroy\` reclaims it." >&2
                exit 1
                ;;
            *)
                # poweroff, not_created, aborted, or a status that could
                # not be read because the machine was never made. None of
                # those is a running VM, which is all this promises.
                ;;
        esac

        ;;
    status)
        # Answers the question that used to need `ps | grep qemu`: is the
        # machine up. Exits 0 when it is running, 1 when it is not, so it
        # is usable in a condition as well as readable by a person.
        state=$(cd "$VAGRANT_DIR" && vagrant status --machine-readable 2>/dev/null \
                | sed -n 's/.*,state,//p' | head -1)
        case "$state" in
            running)
                echo "vm: running"
                ;;
            *)
                echo "vm: not running (${state:-no machine})"
                exit 1
                ;;
        esac
        ;;
    hold)
        # "I want this machine up; do not reap it."
        #
        # Distinct from KEEP_VM, which says "do not tear down between
        # the steps of THIS run". This one outlives the process that set
        # it, because the thing it records also does: a person who ran
        # `chore vm:up` to work in the guest.
        #
        # A lifecycle hook can see `{{.TASK}}`, the invoked task's name,
        # so it is fair to ask why a file on disk is needed at all. It
        # is because that only describes the CURRENT invocation. The
        # machine has to survive the next one, and the one after that.
        mkdir -p "$(dirname "$HOLD")"
        : > "$HOLD"
        echo 'vm: held — reap will leave it running until vm.sh down.' >&2
        ;;
    reap)
        # The safety net, run from chores.yml as `lifecycle: after_all`
        # so ANY chore invocation cleans up a machine that nothing else
        # did.
        #
        # `defer:` is the primary teardown and stays the primary
        # teardown: it fails the task when it fails, which is what makes
        # a leak impossible to miss. But it only covers a task that
        # reached the step that registered it. It cannot cover a bare
        # `cargo test` — several suites shell out to this script and
        # boot the machine — or a run that was killed outright. This
        # can, on the next chore invocation of any kind.
        #
        # It fails SOFT, and that is deliberate rather than a
        # limitation: chore prints an after_all failure without failing
        # the run, which is right for a net. Turning an unrelated
        # `chore build` red because somebody else left a VM up would
        # teach people to ignore it.
        #
        # The probe is a process check, not `vagrant status`, because
        # this runs on every invocation: ~20ms against ~1s, which is
        # what makes it affordable to have always on.
        if ! pgrep -f "$VAGRANT_DIR/.vagrant/machines" >/dev/null 2>&1; then
            exit 0
        fi
        if [ -f "$HOLD" ]; then
            echo "[vm] left running: it was asked for with \`chore vm:up\`. \`chore vm:down\` stops it." >&2
            exit 0
        fi
        echo "vm: a machine was left running by something that did not clean up — stopping it." >&2
        echo "    (a bare \`cargo test\` boots it; \`chore test\` tears it down.)" >&2
        exec "${BASH_SOURCE[0]}" down
        ;;
    destroy)
        rm -f "$HOLD"
        (cd "$VAGRANT_DIR" && vagrant destroy -f)
        ;;
    *)
        sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
esac
