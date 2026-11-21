#!/usr/bin/env bash
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/vm-slot-boot-observation.XXXXXX")"
trap 'rm -r "$sandbox"' EXIT
fails=0
ok() { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

mkdir -p "$sandbox/repo/tests/scripts" "$sandbox/repo/scripts"
ln -s "$REPO/../fs-linux-test-harness" "$sandbox/fs-linux-test-harness"
cp "$REPO/fs-linux-test-harness.toml" "$sandbox/repo/"
sed 's/echo absent > "\$FLTH_TEST_STUB\/state"/echo "${BOOT_OBSERVATION_NEXT:-absent}" > "\$FLTH_TEST_STUB\/state"/' \
    "$REPO/tests/scripts/vm-slot-after-a-run.sh" > "$sandbox/repo/tests/scripts/vm-slot-after-a-run.sh"
cp "$REPO/scripts/test.sh" "$sandbox/repo/scripts/test.sh"

for initial_state in absent stopped; do
    if BOOT_OBSERVATION_NEXT="$initial_state" bash "$sandbox/repo/tests/scripts/vm-slot-after-a-run.sh" > "$sandbox/$initial_state.log" 2>&1; then
        ok "a run joins a guest initially $initial_state and ends its session"
    else
        cat "$sandbox/$initial_state.log" >&2
        fail "a run joins a guest initially $initial_state and ends its session"
    fi
done

sed 's|"$HARNESS/scripts/vm.sh" session cargo test --features cli "$@"|cargo test --features cli "$@"|' \
    "$REPO/scripts/test.sh" > "$sandbox/repo/scripts/test.sh"
if bash "$sandbox/repo/tests/scripts/vm-slot-after-a-run.sh" > "$sandbox/no-session.log" 2>&1; then
    fail "the guard rejects a runner that bypasses the harness session"
elif grep -Fq 'not ok a passing run: cargo ran in a completed harness session' "$sandbox/no-session.log"; then
    ok "the guard rejects a runner that bypasses the harness session"
else
    cat "$sandbox/no-session.log" >&2
    fail "the broken runner fails specifically for bypassing the harness session"
fi

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails vm-slot-boot-observation violation(s)" >&2
    exit 1
fi
echo "PASS  guest state does not bypass session ownership"
