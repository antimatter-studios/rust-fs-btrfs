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

awk '
    /^vm_state\(\)/ {
        print "vm_state() {"
        print "    local state"
        print "    state=\"$(cat \"$FLTH_TEST_STUB/state\")\""
        print "    if [ \"${observing_kill:-0}\" = 1 ]; then"
        print "        if [ -f \"$FLTH_TEST_STUB/observed-running\" ]; then"
        print "            printf \"%s\\n\" \"$BOOT_OBSERVATION_NEXT\""
        print "            return"
        print "        fi"
        print "        if [ \"$state\" = running ]; then"
        print "            : > \"$FLTH_TEST_STUB/observed-running\""
        print "        fi"
        print "    fi"
        print "    printf \"%s\\n\" \"$state\""
        print "}"
        next
    }
    /^set -m$/ { print "observing_kill=1" }
    /^kill -TERM/ { print "observing_kill=0" }
    { print }
' "$REPO/tests/scripts/vm-slot-after-a-run.sh" > "$sandbox/repo/tests/scripts/vm-slot-after-a-run.sh"

for next_state in stopped running; do
    cp "$REPO/scripts/test.sh" "$sandbox/repo/scripts/test.sh"
    if BOOT_OBSERVATION_NEXT="$next_state" bash "$sandbox/repo/tests/scripts/vm-slot-after-a-run.sh" > "$sandbox/$next_state.log" 2>&1; then
        ok "a boot observed running still counts when the next sample is $next_state"
    else
        cat "$sandbox/$next_state.log" >&2
        fail "a boot observed running still counts when the next sample is $next_state"
    fi
done

sed -e 's/\. "\$1" || exit 1;/:;/' \
    -e '/^"${session\[@\]}" cargo test /a\
exit "$rc"
' "$REPO/scripts/test.sh" > "$sandbox/repo/scripts/test.sh"
if BOOT_OBSERVATION_NEXT=running bash "$sandbox/repo/tests/scripts/vm-slot-after-a-run.sh" > "$sandbox/leak.log" 2>&1; then
    fail "the guard rejects a copy with session teardown and recovery disabled"
elif grep -Fq 'not ok a passing run: the slot is free afterwards (status says: held by ' "$sandbox/leak.log" &&
     grep -Fq 'not ok a passing run: and the VM it booted is down (state: running)' "$sandbox/leak.log"; then
    ok "the guard rejects a copy that leaks both the VM and its slot"
else
    cat "$sandbox/leak.log" >&2
    fail "the broken copy fails specifically for its running VM and held slot"
fi

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails vm-slot-boot-observation violation(s)" >&2
    exit 1
fi
echo "PASS  boot observations survive provisioning shutdown, and real slot leaks still fail"
