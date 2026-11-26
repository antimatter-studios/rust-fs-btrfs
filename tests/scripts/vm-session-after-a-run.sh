#!/usr/bin/env bash
# The v0.7.0 session ends each run and detaches its tree. A test runner
# must preserve cargo's status without stopping or destroying the shared guest.
# Exercise the real harness scripts against their stub engine; no VM boots.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HARNESS="${FLTH_TEST_HARNESS:-$REPO/../fs-linux-test-harness}"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# NOTHING SKIPS: without the harness sibling this proves nothing, so it
# fails and names the task that provides it.
for need in scripts/vm.sh scripts/vm-session.sh tests/stubs/engine.sh; do
    if [ ! -f "$HARNESS/$need" ]; then
        echo "not ok the fs-linux-test-harness sibling has no $need -- run 'chore siblings'" >&2
        echo "FAIL  vm-session-after-a-run could not run" >&2
        exit 1
    fi
done

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/vm-session-after-a-run.XXXXXX")"
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
host_tools() { printf '#!/usr/bin/env bash\nexit %s\n' "$1" > "$H/scripts/host-tools.sh"; }
host_tools 0

# What an oracle test process does with the VM, then how it ends.
cat > "$sandbox/bin/cargo" <<STUB
#!/usr/bin/env bash
if [ "\${CARGO_STUB:-}" = no-vm ]; then exit 0; fi
if ! "$H/scripts/vm.sh" up > "\$FLTH_TEST_STUB/up.log" 2>&1; then
    echo "the stand-in cargo's vm.sh up failed:" >&2
    cat "\$FLTH_TEST_STUB/up.log" >&2
    exit 1
fi
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
# The harness's memory admission, at CI's 4 GiB guest, answered by this guard
# and not by the machine running it.
export FLTH_SHARED_MEMORY=4G
# shellcheck source=../script-support/host-memory.sh
source "$REPO/tests/script-support/host-memory.sh"
give_host_memory "$sandbox" 65536
unset FLTH_GUEST FS_BTRFS_TEST_TMPDIR
export FLTH_CONFIG="$SUT/fs-linux-test-harness.toml"

reset() {
    echo absent > "$FLTH_TEST_STUB/state"
    : > "$FLTH_TEST_STUB/calls"
    rm -f "$FLTH_TEST_STUB/cargo-ready"
}
vm_state() { cat "$FLTH_TEST_STUB/state"; }
run() { (cd "$SUT" && scripts/test.sh --test oracle_stand_in) > "$sandbox/out" 2>&1; }
expect_finished() {
    local label="$1" machine sessions markers
    machine="$("$H/scripts/vm.sh" config | sed -n 's/^machine=//p')"
    [ -n "$machine" ] && [ -d "$machine" ] && ok "$label: resolved shared machine" || fail "$label: shared machine directory missing"
    sessions="$machine/sessions"
    markers="$(find "$sessions" -mindepth 1 -maxdepth 1 -type f 2>/dev/null)"
    [ -z "$markers" ] && ok "$label: no session marker remains" || fail "$label: session markers remain: $markers"
    [ "$(vm_state)" = running ] && ok "$label: shared guest remains running" || fail "$label: guest state is $(vm_state)"
    grep -q '^attach ' "$FLTH_TEST_STUB/calls" && ok "$label: run attached its tree" || fail "$label: run did not attach its tree"
    grep -q '^detach ' "$FLTH_TEST_STUB/calls" && ok "$label: run detached its tree" || fail "$label: run did not detach its tree"
    if grep -Eq '^(down|destroy)( |$)' "$FLTH_TEST_STUB/calls"; then
        fail "$label: test.sh stopped or destroyed the shared guest"
    else
        ok "$label: no guest stop or destroy"
    fi
}

reset
CARGO_STUB=pass run; rc=$?
if [ "$rc" = 0 ]; then
    ok "a passing run exits 0"
else
    fail "a passing run exits 0 (got $rc)"
    cat "$sandbox/out" >&2
fi
expect_finished "passing run"

reset
CARGO_STUB=fail run; rc=$?
[ "$rc" = 101 ] && ok "a failing run keeps cargo's status" || fail "a failing run keeps cargo's status (got $rc)"
expect_finished "failing run"

reset
set -m
(cd "$SUT" && CARGO_STUB=hang exec scripts/test.sh --test oracle_stand_in) > "$sandbox/out" 2>&1 &
pid=$!
set +m
tries=0
until [ -f "$FLTH_TEST_STUB/cargo-ready" ] || [ "$tries" -ge 200 ]; do
    sleep 0.05; tries=$(( tries + 1 ))
done
[ -f "$FLTH_TEST_STUB/cargo-ready" ] && ok "TERM run reached cargo" || fail "TERM run did not reach cargo"
kill -TERM -- "-$pid" 2>/dev/null || true
wait "$pid"; rc=$?
[ "$rc" != 0 ] && ok "TERM run does not report success" || fail "TERM run reported success"
expect_finished "TERM run"

reset
FLTH_GUEST=1 CARGO_STUB=no-vm run; rc=$?
[ "$rc" = 0 ] && ok "guest run exits 0" || fail "guest run exits $rc"
[ ! -s "$FLTH_TEST_STUB/calls" ] && ok "guest run makes no harness call" || fail "guest run called harness"
reset
host_tools 1
CARGO_STUB=no-vm run; rc=$?
[ "$rc" = 0 ] && ok "VM-ineligible host run exits 0" || fail "VM-ineligible host run exits $rc"
[ ! -s "$FLTH_TEST_STUB/calls" ] && ok "VM-ineligible host makes no harness call" || fail "VM-ineligible host called harness"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails shared-guest session violation(s)" >&2
    exit 1
fi
echo "PASS  each test session ends its run and preserves the shared guest"
