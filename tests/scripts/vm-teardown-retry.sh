#!/usr/bin/env bash
# vm-teardown-retry.sh — the harness owns session cleanup, while this
# wrapper preserves Cargo's status and never destroys the shared guest.
# The harness may leave the guest running for the next test; only an
# explicit `vm:down` stops it. A session-end failure is still reported.
#
#   bash tests/scripts/vm-teardown-retry.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HARNESS="$REPO/../fs-linux-test-harness"
fails=0
ok()   { :; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# NOTHING SKIPS: without the harness sibling this proves nothing.
for need in scripts/vm.sh scripts/vm-session.sh tests/stubs/engine.sh; do
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
mv "$H/scripts/vm.sh" "$H/scripts/vm-real.sh"
cat > "$H/scripts/vm.sh" <<'WRAPPER'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FLTH_TEST_STUB/vm-calls"
if [ "${1:-}" = session-end ] && [ -f "$FLTH_TEST_STUB/session-end-fails" ]; then
    "$(dirname "$0")/vm-real.sh" "$@" || exit $?
    exit 1
fi
exec "$(dirname "$0")/vm-real.sh" "$@"
WRAPPER

cat > "$sandbox/bin/cargo" <<STUB
#!/usr/bin/env bash
if ! "$H/scripts/vm.sh" up > "\$FLTH_TEST_STUB/up.log" 2>&1; then
    echo "the stand-in cargo's vm.sh up failed:" >&2
    cat "\$FLTH_TEST_STUB/up.log" >&2
    exit 1
fi
case "\${CARGO_STUB:-pass}" in
    pass) exit 0 ;;
    fail) exit 101 ;;
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
unset FLTH_SHARED_CPUS FLTH_SHARED_DISK
# The harness's memory admission, at CI's 4 GiB guest, answered by this guard
# and not by the machine running it.
export FLTH_SHARED_MEMORY=4G
# shellcheck source=../script-support/host-memory.sh
source "$REPO/tests/script-support/host-memory.sh"
give_host_memory "$sandbox" 65536

reset() {
    rm -rf "$FLTH_CACHE_DIR/machines"
    echo absent > "$FLTH_TEST_STUB/state"
    : > "$FLTH_TEST_STUB/calls"
    : > "$FLTH_TEST_STUB/vm-calls"
    rm -f "$FLTH_TEST_STUB/session-end-fails"
}
vm_state() { cat "$FLTH_TEST_STUB/state"; }
run() { (cd "$SUT" && scripts/test.sh --test oracle_stand_in) > "$sandbox/out" 2>&1; }

# --- 1. A successful run ends its session, not the shared guest. ---------
reset
CARGO_STUB=pass run; rc=$?
if [ "$rc" = 0 ]; then
    ok
else
    fail "a passing run exits 0 (got $rc)"
    cat "$sandbox/out" >&2
fi
[ "$(vm_state)" = running ] && ok || fail "the shared guest stays running (state: $(vm_state))"
grep -q '^session-end ' "$FLTH_TEST_STUB/vm-calls" && ok || fail "the harness ends the run's session"
grep -qE '^(down|destroy)( |$)' "$FLTH_TEST_STUB/vm-calls" && fail "the run never stops or destroys the shared guest" || ok

# --- 2. A failing run keeps Cargo's status and the shared guest. ----------
reset
CARGO_STUB=fail run; rc=$?
[ "$rc" = 101 ] && ok || fail "a failing run keeps cargo's status (got $rc)"
[ "$(vm_state)" = running ] && ok || fail "a failing run leaves the shared guest running (state: $(vm_state))"
grep -q '^session-end ' "$FLTH_TEST_STUB/vm-calls" && ok || fail "the harness ends the failed run's session"

# --- 3. A session-end failure is reported without destroying the guest. --
reset
touch "$FLTH_TEST_STUB/session-end-fails"
CARGO_STUB=pass run; rc=$?
[ "$rc" != 0 ] && ok || fail "a session-end failure fails a passing run"
grep -qi "SESSION'S END FAILED" "$sandbox/out" && ok || fail "the session-end failure is reported"
[ "$(vm_state)" = running ] && ok || fail "a session-end failure leaves the shared guest running (state: $(vm_state))"
grep -qE '^(down|destroy)( |$)' "$FLTH_TEST_STUB/vm-calls" && fail "session cleanup never stops or destroys the shared guest" || ok

# --- 4. A failing command keeps its own status if teardown also fails. ----
reset
touch "$FLTH_TEST_STUB/session-end-fails"
CARGO_STUB=fail run; rc=$?
[ "$rc" = 101 ] && ok || fail "cargo's failure survives a session-end failure (got $rc)"
grep -qi "SESSION'S END FAILED" "$sandbox/out" && ok || fail "the session-end failure remains visible"
[ "$(vm_state)" = running ] && ok || fail "a failed run leaves the shared guest running (state: $(vm_state))"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails vm-teardown-retry violation(s)" >&2
    exit 1
fi
echo "PASS  the harness owns session cleanup and the shared guest survives each run"
