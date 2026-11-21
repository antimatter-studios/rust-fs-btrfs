#!/usr/bin/env bash
#
# vm-slot-after-a-run.sh — a test run ends its harness session however it
# ends, leaving the shared guest available to the next run (#141).
#
# THE LEAK. An oracle or kernel test boots the VM from inside its own
# process (`vm.sh up`, tests/support/src/oracle.rs). The harness records
# each run as a session so `vm:down` cannot stop the shared guest while a
# run is active. A finished run must release its session, but leave the
# guest up for the next consumer.
#
# What is driven here is the real scripts/test.sh and the real harness
# scripts (vm.sh, vm-session.sh) from the pinned sibling,
# with only the harness's engine swapped for its own stub — so no VM, no
# KVM and no root. `cargo` is a stand-in that does what an oracle test
# process does: `vm.sh up`, then pass, fail, or wait to be killed.
#
# And the negative arm: a run must never end another repository's
# session or stop the shared guest while that session is active.
#
#   bash tests/scripts/vm-slot-after-a-run.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HARNESS="$REPO/../fs-linux-test-harness"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# NOTHING SKIPS: without the harness sibling this proves nothing, so it
# fails and names the task that provides it.
for need in scripts/vm.sh scripts/vm-session.sh tests/stubs/engine.sh; do
    if [ ! -f "$HARNESS/$need" ]; then
        echo "not ok the fs-linux-test-harness sibling has no $need -- run 'chore siblings'" >&2
        echo "FAIL  vm-slot-after-a-run could not run" >&2
        exit 1
    fi
done

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/vm-slot-after-a-run.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT

# The layout test.sh resolves its sibling from: <dir>/repo beside
# <dir>/fs-linux-test-harness.
SUT="$sandbox/repo"
H="$sandbox/fs-linux-test-harness"
mkdir -p "$SUT/scripts" "$H" "$sandbox/bin" "$sandbox/stub"
cp "$REPO/scripts/test.sh" "$SUT/scripts/"
cp "$REPO/fs-linux-test-harness.toml" "$SUT/"
# The setup script runs on the host under the stub engine; it must do nothing.
printf '#!/usr/bin/env bash\nexit 0\n' > "$SUT/scripts/vm-setup.sh"
cp -R "$HARNESS/scripts" "$H/"
cp "$HARNESS/tests/stubs/engine.sh" "$H/scripts/lib/engine.sh"
mv "$H/scripts/vm.sh" "$H/scripts/vm-real.sh"
cat > "$H/scripts/vm.sh" <<'WRAPPER'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FLTH_TEST_STUB/vm-calls"
exec "$(dirname "$0")/vm-real.sh" "$@"
WRAPPER
host_tools() { printf '#!/usr/bin/env bash\nexit %s\n' "$1" > "$H/scripts/host-tools.sh"; }
host_tools 0

# What an oracle test process does with the VM, then how it ends.
cat > "$sandbox/bin/cargo" <<STUB
#!/usr/bin/env bash
if [ "\${CARGO_STUB:-}" = no-vm ]; then exit 0; fi
"$H/scripts/vm.sh" up >/dev/null 2>&1 || exit 1
case "\${CARGO_STUB:-pass}" in
    pass) exit 0 ;;
    fail) exit 101 ;;
    hang) : > "\$FLTH_TEST_STUB/cargo-ready"; sleep 30 & wait \$! ;;
esac
STUB
chmod +x "$sandbox/bin/cargo" "$SUT/scripts/test.sh" "$H/scripts/"*.sh

export PATH="$sandbox/bin:$PATH"
export FLTH_TEST_STUB="$sandbox/stub"
# The stub engine runs guest scripts with this interpreter (harness v0.5.4).
export FLTH_TEST_GUEST_BASH="${FLTH_TEST_GUEST_BASH:-bash}"
export FLTH_CACHE_DIR="$sandbox/cache"
export FLTH_STATE_DIR="$sandbox/state"
unset FLTH_CONFIG FLTH_KEEP_VM FLTH_GUEST FLTH_SLOT_WAIT FLTH_SLOT_BOOT_GRACE FS_BTRFS_TEST_TMPDIR

reset() { rm -rf "$FLTH_CACHE_DIR/machines"; echo absent > "$FLTH_TEST_STUB/state"; : > "$FLTH_TEST_STUB/calls"; : > "$FLTH_TEST_STUB/vm-calls"; }
vm_cmd() { (cd "$SUT" && "$H/scripts/vm.sh" "$@"); }
vm_state() { cat "$FLTH_TEST_STUB/state"; }
run() { (cd "$SUT" && scripts/test.sh --test oracle_stand_in) >/dev/null 2>&1; }

expect_session_ended() {
    # expect_session_ended <how the run ended>
    if grep -q '^session cargo test ' "$FLTH_TEST_STUB/vm-calls" &&
        grep -q '^session-begin ' "$FLTH_TEST_STUB/vm-calls" &&
        grep -q '^session-end ' "$FLTH_TEST_STUB/vm-calls"; then
        ok "$1: cargo ran in a completed harness session"
    else
        fail "$1: cargo ran in a completed harness session"
    fi
    [ "$(vm_state)" = running ] && ok "$1: the shared guest stays up" \
                                || fail "$1: the shared guest stays up (state: $(vm_state))"
    if vm_cmd down >/dev/null 2>&1; then
        ok "$1: the session ended, so down can stop the guest"
    else
        fail "$1: the session ended, so down can stop the guest"
    fi
    [ "$(vm_state)" = stopped ] && ok "$1: an explicit down stops the guest" \
                                || fail "$1: an explicit down stops the guest (state: $(vm_state))"
}

# --- 1. A run that PASSES ends its session. ------------------------------
reset
CARGO_STUB=pass run; rc=$?
[ "$rc" = 0 ] && ok "a passing run exits 0" || fail "a passing run exits 0 (got $rc)"
expect_session_ended "a passing run"

# --- 2. A run that FAILS gives it back, and keeps its own status. --------
reset
CARGO_STUB=fail run; rc=$?
[ "$rc" = 101 ] && ok "a failing run keeps cargo's status" || fail "a failing run keeps cargo's status (got $rc)"
expect_session_ended "a failing run"

# --- 3. A run KILLED by a signal gives it back. ---------------------------
#
# Sent to the run's whole process group, as Ctrl-C or a job's timeout
# does. Job control gives the background run a group of its own.
reset
set -m
(cd "$SUT" && CARGO_STUB=hang exec scripts/test.sh --test oracle_stand_in) >/dev/null 2>&1 &
pid=$!
set +m
tries=0
until [ -f "$FLTH_TEST_STUB/cargo-ready" ] || [ "$tries" -ge 200 ]; do
    sleep 0.05; tries=$(( tries + 1 ))
done
[ -f "$FLTH_TEST_STUB/cargo-ready" ] && ok "the killed run had booted the VM first" \
                            || fail "the killed run had booted the VM first (state: $(vm_state))"
kill -TERM -- "-$pid" 2>/dev/null
wait "$pid"; rc=$?
[ "$rc" != 0 ] && ok "a run killed by TERM does not report success" \
               || fail "a run killed by TERM does not report success"
expect_session_ended "a run killed by TERM"

# --- 4. A run never ends another live session. ---------------------------
reset
vm_cmd up >/dev/null 2>&1 || fail "the shared guest boots for another session"
sleep 30 & owner=$!
vm_cmd session-begin "$owner" || fail "another live session is registered"
CARGO_STUB=pass run; rc=$?
[ "$rc" = 0 ] && ok "a passing run beside another session exits 0" || fail "a passing run beside another session exits 0 (got $rc)"
vm_cmd down >/dev/null 2>&1 && ok "down leaves the guest up while another session is active" \
                            || fail "down leaves the guest up while another session is active"
[ "$(vm_state)" = running ] && ok "the other session's guest remains running" \
                            || fail "the other session's guest remains running (state: $(vm_state))"
vm_cmd session-end "$owner" || fail "the other live session ends"
kill "$owner" 2>/dev/null
wait "$owner" 2>/dev/null
vm_cmd down >/dev/null 2>&1 && ok "down succeeds after the last session ends" \
                            || fail "down succeeds after the last session ends"

# --- 5. Where there is no VM to own, there is no session. ----------------
#
# In the guest (`chore test:vm`) the suite IS in the VM; on a host that
# cannot run one (CI's unit and aarch64 jobs) there is nothing to bring
# down. Neither may ask the engine anything, or the unit tier would fail
# on a teardown of a VM that could never exist.
reset
FLTH_GUEST=1 CARGO_STUB=no-vm run; rc=$?
[ "$rc" = 0 ] && [ ! -s "$FLTH_TEST_STUB/calls" ] \
    && ok "in the guest the run makes no harness call" \
    || fail "in the guest the run makes no harness call (rc $rc; calls: $(tr '\n' ' ' < "$FLTH_TEST_STUB/calls"))"
reset
host_tools 1
CARGO_STUB=no-vm run; rc=$?
[ "$rc" = 0 ] && [ ! -s "$FLTH_TEST_STUB/calls" ] \
    && ok "on a host that cannot run the VM the run makes no harness call" \
    || fail "on a host that cannot run the VM the run makes no harness call (rc $rc; calls: $(tr '\n' ' ' < "$FLTH_TEST_STUB/calls"))"
host_tools 0

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails vm-slot-after-a-run violation(s)" >&2
    exit 1
fi
echo "PASS  test runs end their own session and leave the shared guest to other runs"
