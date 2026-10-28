#!/usr/bin/env bash
# vm-teardown-retry.sh — a halt that leaves the VM running gets one
# bounded recovery, and a VM that stays up still fails the run.
#
# CI run 38732797449 (x86_64 job, 873eb1c) passed every oracle test and
# then failed the tier: the harness's halt returned with the guest still
# running ("vm: halt did not leave the VM confirmed stopped", then
# "TEARDOWN FAILED"). scripts/test.sh now retries the teardown with the
# steps the harness's message names — the session's end again, then
# `vm.sh destroy` — and reports cargo's own status only once the VM is
# confirmed stopped.
#
# Driven through the harness's real scripts with its stub engine, as
# vm-slot-after-a-run.sh is; the stub's `engine_down` is wrapped in the
# sandbox copy so that a set number of halts leave the guest running.
#
#   bash tests/scripts/vm-teardown-retry.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HARNESS="$REPO/../fs-linux-test-harness"
fails=0
ok()   { :; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# NOTHING SKIPS: without the harness sibling this proves nothing.
for need in scripts/vm.sh scripts/vm-slot.sh scripts/vm-session.sh tests/stubs/engine.sh; do
    if [ ! -f "$HARNESS/$need" ]; then
        echo "not ok the fs-linux-test-harness sibling has no $need -- run 'chore siblings'" >&2
        echo "FAIL  vm-teardown-retry could not run" >&2
        exit 1
    fi
done

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/vm-teardown-retry.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT

SUT="$sandbox/repo"
H="$sandbox/fs-linux-test-harness"
mkdir -p "$SUT/scripts" "$H" "$sandbox/bin" "$sandbox/stub"
cp "$REPO/scripts/test.sh" "$SUT/scripts/"
cp "$REPO/fs-linux-test-harness.toml" "$SUT/"
printf '#!/usr/bin/env bash\nexit 0\n' > "$SUT/scripts/vm-setup.sh"
cp -R "$HARNESS/scripts" "$H/"
cp "$HARNESS/tests/stubs/engine.sh" "$H/scripts/lib/engine.sh"
printf '#!/usr/bin/env bash\nexit 0\n' > "$H/scripts/host-tools.sh"
# A halt that returns with the guest still running, down_failures times
# (each `vm.sh down` is a graceful halt then a forced one, since v0.4.0);
# destroy_fails makes `vm.sh destroy` leave it running as well.
cat >> "$H/scripts/lib/engine.sh" <<'WRAP'

engine_down() {
    local n
    n="$(stub_read down_failures 0)"
    stub_log "down${1:+ $1}"
    # A provisioning boot's halt is the harness's own (v0.3.0), not the
    # session's end: a fault there makes `up` refuse, before cargo runs.
    if [ "$n" -gt 0 ] && [ ! -f "$FLTH_MACHINE_DIR/provisioning" ]; then
        echo $((n - 1)) > "$STUB/down_failures"
        echo running > "$STUB/state"
        return 0
    fi
    echo stopped > "$STUB/state"
}

engine_destroy() {
    if [ -f "$STUB/new_session" ]; then
        if "$FLTH_HARNESS/scripts/vm.sh" session-begin "$(cat "$STUB/new_session")" >/dev/null 2>&1; then
            stub_log new-session-entered
        else
            stub_log new-session-refused
        fi
    fi
    stub_log destroy
    if [ -f "$STUB/destroy_fails" ]; then
        echo running > "$STUB/state"
        return 0
    fi
    echo absent > "$STUB/state"
}
WRAP

cat > "$sandbox/bin/cargo" <<STUB
#!/usr/bin/env bash
"$H/scripts/vm.sh" up >/dev/null 2>&1 || exit 1
case "\${CARGO_STUB:-pass}" in
    pass) exit 0 ;;
    fail) exit 101 ;;
esac
STUB
chmod +x "$sandbox/bin/cargo" "$SUT/scripts/test.sh" "$H/scripts/"*.sh

export PATH="$sandbox/bin:$PATH"
export FLTH_TEST_STUB="$sandbox/stub"
export FLTH_CACHE_DIR="$sandbox/cache"
export FLTH_STATE_DIR="$sandbox/state"
unset FLTH_CONFIG FLTH_KEEP_VM FLTH_GUEST FLTH_SLOT_WAIT FLTH_SLOT_BOOT_GRACE FS_BTRFS_TEST_TMPDIR

reset() {
    rm -rf "$FLTH_STATE_DIR/slot.lock"
    echo absent > "$FLTH_TEST_STUB/state"
    : > "$FLTH_TEST_STUB/calls"
    echo "${1:-0}" > "$FLTH_TEST_STUB/down_failures"
    rm -f "$FLTH_TEST_STUB/destroy_fails" "$FLTH_TEST_STUB/new_session"
}
vm_state() { cat "$FLTH_TEST_STUB/state"; }
run() { (cd "$SUT" && scripts/test.sh --test oracle_stand_in) > "$sandbox/out" 2>&1; }

# --- 1. One halt leaves the VM running; the retry stops it. --------------
reset 2
CARGO_STUB=pass run; rc=$?
[ "$rc" = 0 ] && ok || fail "a passing run whose first halt left the VM running exits 0 once the retry stops it (got $rc)"
[ "$(vm_state)" = stopped ] && ok || fail "the retried halt leaves the VM stopped (state: $(vm_state))"

# --- 2. Same, with cargo failing: cargo's status survives the retry. -----
reset 2
CARGO_STUB=fail run; rc=$?
[ "$rc" = 101 ] && ok || fail "a failing run keeps cargo's status through the retry (got $rc)"
[ "$(vm_state)" = stopped ] && ok || fail "and the VM is stopped (state: $(vm_state))"

# --- 3. Two halts fail; destroy is the last step and confirms it gone. ---
reset 4
CARGO_STUB=pass run; rc=$?
[ "$rc" = 0 ] && ok || fail "a run whose halts both failed exits 0 once destroy confirms the VM gone (got $rc)"
grep -qx destroy "$FLTH_TEST_STUB/calls" && ok || fail "destroy is the step after a failed retry"

# --- 4. A VM that stays up is still fatal, and says so. ------------------
reset 9
touch "$FLTH_TEST_STUB/destroy_fails"
CARGO_STUB=pass run; rc=$?
[ "$rc" != 0 ] && ok || fail "a passing run whose VM never stops fails"
grep -q 'TEARDOWN FAILED' "$sandbox/out" && ok || fail "and names the teardown failure"

# --- 5. Another live session owns the VM: it is never halted or destroyed.
# Harness v0.5.1's `session-end` returns 0 and leaves the VM running when
# another live session marker exists, so the recovery must neither read
# that as a stop nor fall through to `vm.sh destroy`. The marker is the
# harness's own format: "<pid>\t<ps -o lstart=>" in the machine's
# sessions directory, for a process that is alive for the whole run.
reset 9
touch "$FLTH_TEST_STUB/destroy_fails"
project="$(awk '/^\[project\]/{p=1;next} /^\[/{p=0} p && /^name *=/{gsub(/.*= *"|".*/,"");print;exit}' "$SUT/fs-linux-test-harness.toml")"
sessions="$FLTH_CACHE_DIR/machines/$project/sessions"
sleep 300 & owner=$!
mkdir -p "$sessions"
printf '%s\t%s\n' "$owner" "$(ps -o lstart= -p "$owner" | sed 's/^ *//')" > "$sessions/$owner"
echo running > "$FLTH_TEST_STUB/state"
CARGO_STUB=pass run; rc=$?
[ -n "$project" ] && ok || fail "the consumer config names its project"
[ "$rc" = 0 ] && ok || fail "a passing run beside another live session exits 0 (got $rc)"
grep -qx destroy "$FLTH_TEST_STUB/calls" && fail "destroy never runs while another live session owns the VM" || ok
grep -q '^down' "$FLTH_TEST_STUB/calls" && fail "no halt runs while another live session owns the VM" || ok
[ "$(vm_state)" = running ] && ok || fail "the other session's VM is left running (state: $(vm_state))"
kill "$owner" 2>/dev/null; wait "$owner" 2>/dev/null
rm -f "$sessions/$owner"

# --- 6. A new session races with destroy: entry is refused atomically. ---
# The original checks above are unchanged. This contender attempts the
# real harness session-begin at the last destructive boundary, rather
# than merely being present when recovery first checks the registry.
reset 4
echo "$$" > "$FLTH_TEST_STUB/new_session"
CARGO_STUB=pass run; rc=$?
[ "$rc" = 0 ] && ok || fail "reserved recovery still finishes successfully (got $rc)"
grep -qx new-session-refused "$FLTH_TEST_STUB/calls" && ok || fail "a racing session cannot enter during destroy"
grep -qx new-session-entered "$FLTH_TEST_STUB/calls" && fail "destroy never admits a competing session" || ok
[ "$(vm_state)" = absent ] && ok || fail "exclusive destroy confirms the VM absent"
[ -w "$sessions" ] && ok || fail "recovery restores session registry access"

# --- 7. Destroy recovery cannot turn a failed oracle into success. ------
reset 4
CARGO_STUB=fail run; rc=$?
[ "$rc" = 101 ] && ok || fail "cargo's failure survives destroy recovery (got $rc)"
[ "$(vm_state)" = absent ] && ok || fail "failed cargo still receives confirmed teardown"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails vm-teardown-retry violation(s)" >&2
    exit 1
fi
echo "PASS  a halt that leaves the VM running is retried, and a VM that stays up still fails the run"
