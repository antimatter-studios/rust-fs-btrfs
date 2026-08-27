#!/usr/bin/env bash
#
# vm-reap-semantics.sh — what `vm.sh reap` does, and does not, stop.
#
# The reaper runs from `lifecycle: after_all`, so it fires on EVERY
# chore invocation. That makes two of its decisions load-bearing:
#
#   - it must stop a machine nothing cleaned up, which is the whole
#     point;
#   - it must NOT stop one someone asked for with `chore vm:up`, or the
#     net would undo the thing the user just requested.
#
# Getting the second wrong is worse than not having a net at all, and
# neither is visible without a VM to try it on — so `pgrep` and
# `vagrant` are stubbed here and the assertions are on what reap would
# have done.
#
#   bash tests/scripts/vm-reap-semantics.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

# A fake repo laid out the way vm.sh expects, since it derives both the
# vagrant directory and the hold marker from its own location.
mkdir -p "$sandbox/scripts" "$sandbox/tests/vagrant/debian/.vagrant" "$sandbox/bin"
cp "$REPO/scripts/vm.sh" "$sandbox/scripts/"
HOLD="$sandbox/tests/vagrant/debian/.vagrant/keep-running"
LOG="$sandbox/vagrant.log"

# `vagrant` records what it was asked to do; `status` reports a stopped
# machine so `down`'s confirmation passes.
cat > "$sandbox/bin/vagrant" <<EOF
#!/usr/bin/env bash
echo "vagrant \$*" >> "$LOG"
[ "\${1:-}" = "status" ] && echo "default,state,poweroff"
exit 0
EOF
chmod +x "$sandbox/bin/vagrant"

# $1 = exit status the stubbed pgrep returns (0 = a machine is running)
set_pgrep() {
    cat > "$sandbox/bin/pgrep" <<EOF
#!/usr/bin/env bash
exit $1
EOF
    chmod +x "$sandbox/bin/pgrep"
}

reap() {
    : > "$LOG"
    PATH="$sandbox/bin:$PATH" "$sandbox/scripts/vm.sh" reap >"$sandbox/out" 2>&1
    echo $?
}

halted() { grep -q 'vagrant halt' "$LOG" && echo yes || echo no; }

check() {
    local got=$1 want=$2 what=$3
    if [ "$got" = "$want" ]; then
        printf 'ok    %s\n' "$what"
    else
        printf 'FAIL  %s (expected %s, got %s)\n' "$what" "$want" "$got"
        printf '      output: %s\n' "$(cat "$sandbox/out")"
        fails=$((fails + 1))
    fi
}

# Nothing running: the fast path. It must not shell out to vagrant at
# all, because this runs on every chore invocation including ones that
# have nothing to do with the VM.
set_pgrep 1
rm -f "$HOLD"
code=$(reap)
check "$code" "0" "no machine running: succeeds"
check "$(halted)" "no" "no machine running: does not call vagrant"

# A machine nobody accounted for. This is the leak the net exists for.
set_pgrep 0
rm -f "$HOLD"
code=$(reap)
check "$code" "0" "leaked machine: succeeds"
check "$(halted)" "yes" "leaked machine: IS stopped"
if grep -q 'did not clean up' "$sandbox/out"; then
    printf 'ok    leaked machine: says why it acted\n'
else
    printf 'FAIL  leaked machine: reaped silently\n'; fails=$((fails + 1))
fi

# Held: someone ran `chore vm:up` and is working in it. The net must
# keep its hands off.
set_pgrep 0
: > "$HOLD"
code=$(reap)
check "$code" "0" "held machine: succeeds"
check "$(halted)" "no" "held machine: is LEFT RUNNING"
if grep -q 'vm:up' "$sandbox/out"; then
    printf 'ok    held machine: says how to stop it\n'
else
    printf 'FAIL  held machine: silent about why it was left\n'; fails=$((fails + 1))
fi

# Stopping clears the hold: a halted machine is not one anybody is
# working in, and a marker left behind would disarm the net for good.
set_pgrep 1
: > "$HOLD"
PATH="$sandbox/bin:$PATH" "$sandbox/scripts/vm.sh" down >/dev/null 2>&1
if [ -f "$HOLD" ]; then
    printf 'FAIL  down leaves the hold marker, disarming the net permanently\n'
    fails=$((fails + 1))
else
    printf 'ok    down clears the hold\n'
fi

if [ "$fails" -eq 0 ]; then
    echo "PASS  vm reap semantics"
else
    echo "FAIL  $fails assertion(s)" >&2
    exit 1
fi
