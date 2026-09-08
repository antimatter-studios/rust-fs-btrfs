#!/usr/bin/env bash
#
# vm-deadline-semantics.sh — the guest's own poweroff timer.
#
# The oracle VM schedules its own shutdown at boot. That exists because
# every other teardown here is passive: `defer:` needs the task to reach
# the step that registered it, `vm.sh reap` needs a later chore
# invocation, and the slot's age check runs only inside `acquire` and so
# needs another repository to want the slot. On 2026-09-07 none of those
# happened and the machine stayed up for thirteen hours holding the
# global lock, with every net behaving exactly as written.
#
# WHAT THIS TEST CAN AND CANNOT REACH. It cannot boot a VM, so it does
# not assert that a shutdown is really scheduled -- that was verified by
# hand in a live guest and the numbers are in the Vagrantfile's comment.
# What it can do is pin the two things that were WRONG when the
# mechanism was first written, both of which were silent:
#
#   1. the minutes were read from the guest's environment, which Vagrant
#      does not forward, so the knob reported the default and could not
#      be changed. Exporting 90 produced "powering off in 480".
#
#   2. `hold` invoked `vagrant ssh` without the `cd "$VAGRANT_DIR"` that
#      every other call in vm.sh uses, so it could not reach the guest
#      and the deadline it claimed to cancel still stood.
#
# Both were caught only by running the thing. A test that cannot boot a
# guest can still refuse to let them come back.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VAGRANTFILE="$REPO/tests/vagrant/debian/Vagrantfile"
VM_SH="$REPO/scripts/vm.sh"
fails=0

ok()   { printf 'ok    %s\n' "$1"; }
bad()  { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# 1. The deadline must be armed on every boot, not only on the first
#    provision. Without `run: "always"` a machine that was provisioned
#    before this existed -- or one merely halted and restarted -- comes
#    up with no timer at all, which is the exact state that leaked.
check "the deadline provisioner runs on every boot" \
  "grep -q 'provision \"deadline\", type: \"shell\", run: \"always\"' '$VAGRANTFILE'"

# 2. The minutes are interpolated by Ruby on the host. A shell expansion
#    inside the inline script reads the GUEST's environment, which is
#    empty, so the value silently falls back to the default.
check "the minutes are interpolated on the host, not expanded in the guest" \
  "grep -q 'MINS=\"#{deadline_mins}\"' '$VAGRANTFILE'"

check "no shell expansion of the deadline variable survives in the inline script" \
  "! grep -q 'MINS=\"\\\${AM_ORACLE_VM_DEADLINE_MINS' '$VAGRANTFILE'"

check "the host reads the override through ENV.fetch" \
  "grep -q 'ENV.fetch(\"AM_ORACLE_VM_DEADLINE_MINS\"' '$VAGRANTFILE'"

# 3. A previous timer must be cancelled before a new one is scheduled,
#    or a re-provision stacks two and the earlier one wins -- a machine
#    that dies sooner than the number it just printed.
# Anchored to the indented code line, not the bare string: the first
# version of this matched the word `shutdown -c` in the comment twelve
# lines above and passed with the actual call deleted. Which is the
# defect this whole repository keeps finding -- an assertion whose result
# does not depend on the thing it claims to check.
check "a previous timer is cancelled before a new one is set" \
  "grep -qE '^ +shutdown -c >/dev/null' '$VAGRANTFILE'"

# 4. The hold marker lives on tmpfs. In /etc it would survive a reboot,
#    so a machine held once would never re-arm and the exemption would
#    outlive everyone's memory of granting it.
check "the hold marker is on tmpfs so a reboot clears it" \
  "grep -q '/run/am-oracle-vm-held' '$VAGRANTFILE'"

check "the hold marker is not somewhere persistent" \
  "! grep -q '/etc/am-oracle-vm-held' '$VAGRANTFILE' '$VM_SH'"

# 5. `vm.sh hold` has to cancel the guest's timer as well as reap. A
#    hold that stopped reap but let the machine power itself off would be
#    a hold in name only.
check "hold cancels the guest deadline as well as reap" \
  "grep -q 'am-oracle-vm-held' '$VM_SH'"

# 6. And it must reach the guest the way everything else in this file
#    does. A bare `vagrant ssh` finds no machine outside VAGRANT_DIR and
#    fails silently into the error branch.
check "hold reaches the guest from VAGRANT_DIR" \
  "grep -A3 'am-oracle-vm-held' '$VM_SH' | grep -q 'cd \"\\\$VAGRANT_DIR\"'"

# 7. When it cannot reach the guest it must say the deadline still
#    stands. Reporting a hold it did not achieve is worse than failing,
#    because the machine then dies under someone who was told it would
#    not.
check "an unreachable guest is reported rather than assumed held" \
  "grep -qi 'deadline still stands' '$VM_SH'"

echo
if [ "$fails" = 0 ]; then
    echo "PASS  vm deadline semantics"
else
    echo "FAIL  $fails assertion(s)"
    exit 1
fi
