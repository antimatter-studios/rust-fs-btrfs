#!/usr/bin/env bash
#
# output-budget.sh — THE OUTPUT RULE, CHECKED: every test tier is
# budgeted, a budget can actually fail a run, and the wrapper enforcing it
# is the one rust-fs-core publishes.
#
# Quiet-by-default is a convention, and a convention rots in a week. This
# is the number that fails the build instead: a tier added later without
# going through scripts/tier.sh, or given a budget of zero (which
# output-budget.sh reads as "no budget"), fails here rather than being
# noticed the next time somebody scrolls past three thousand lines.
#
# WHAT CHANGED WHEN THE WRAPPER MOVED TO CORE. This file used to run
# ../fs-linux-test-harness/scripts/output-budget.sh directly, which tested a
# script no tier ran any more. Everything below goes through scripts/tier.sh,
# so what is proved is the path the tiers actually take: resolution from core,
# verification by --version, and the four exit shapes.
#
# NOTHING SKIPS. A missing core is not a reason to stop early — it is the
# failure, and it names 'chore siblings' as what provides one.
#
#   bash tests/scripts/output-budget.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# --- 1. Every tier runs its tests through tier.sh, with a real budget. ---
#
# The tiers, by task name. This list is the contract; a tier not named
# here is not checked, so adding one means adding it here too — which is
# the one registration this file asks for, and it is in the same file as
# the assertion.
for tier in test:unit test:images test:oracle test:kernel test:vm test:scripts; do
    block="$(awk -v t="  $tier:" '
        $0 == t { inside = 1; next }
        inside && /^  [a-z][a-z:_-]*:$/ { inside = 0 }
        inside { print }
    ' "$REPO/chores.yml")"

    if [ -z "$block" ]; then
        fail "chores.yml has a task '$tier'"
        continue
    fi
    case "$block" in
        *scripts/tier.sh*) ok "$tier runs through scripts/tier.sh" ;;
        *) fail "$tier runs through scripts/tier.sh, so its output is bounded" ;;
    esac
    # tier.sh LABEL LOG MAX-LINES MAX-BYTES: a zero in either position is
    # "no budget" to output-budget.sh, which is the shape this refuses.
    if printf '%s\n' "$block" | grep -Eq "scripts/tier\.sh +[^ ]+ +[^ ]+ +[1-9][0-9]* +[1-9][0-9]*"; then
        ok "$tier carries a non-zero line and byte budget"
    else
        fail "$tier carries a non-zero line and byte budget"
    fi
done

# --- 2. The wrapper a tier uses comes from rust-fs-core. ----------------
#
# tier.sh resolves it — the ../rust-fs-core sibling first, then the
# am-fs-core package root cargo reports — and holds whatever it finds to
# `--version`. This asserts the source rather than a checksum: a digest
# recorded here would have to be updated here for every edit to core, in
# every repository that records one, which is the lockstep moving the
# wrapper into core removed.
tier="$REPO/scripts/tier.sh"
if [ ! -x "$tier" ]; then
    fail "scripts/tier.sh is executable"
else
    ok "scripts/tier.sh is executable"
fi

sibling="$REPO/../rust-fs-core/scripts/output-budget.sh"
if [ -f "$sibling" ]; then
    got="$(bash "$sibling" --version 2>/dev/null)"
    [ "$got" = "rust-fs-core-output-budget 1" ] \
        && ok "the ../rust-fs-core sibling publishes the wrapper at API 1" \
        || fail "the ../rust-fs-core sibling publishes the wrapper at API 1 (got '$got')"
else
    # Not a skip: the tiers cannot run at all without this, so neither can
    # its test.
    fail "../rust-fs-core/scripts/output-budget.sh is present -- run 'chore siblings' (pin: v0.2.13 or later)"
fi

# Comments stripped first: tier.sh's header names the old harness path on
# purpose, to tell a reader where it went. A CODE line naming it is the defect.
if grep -v '^[[:space:]]*#' "$tier" | grep -q 'fs-linux-test-harness'; then
    fail "tier.sh no longer reads the wrapper out of the harness"
else
    ok "tier.sh no longer reads the wrapper out of the harness"
fi

# --- 3. The four exit shapes, THROUGH tier.sh. --------------------------
mkdir -p "$REPO/tmp"
work="$(mktemp -d "$REPO/tmp/output-budget-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT

run_tier() { ( cd "$REPO" && bash scripts/tier.sh "$@" ) 2>&1; }

# Passed, but printed too much: status 65, distinct from a failing suite.
out="$(run_tier ob-loud ob-loud 5 0 -- \
        sh -c 'i=0; while [ $i -lt 40 ]; do echo line $i; i=$((i+1)); done')"
rc=$?
[ "$rc" = 65 ] && ok "a tier over its line budget exits 65" \
               || fail "a tier over its line budget exits 65 (got $rc)"
case "$out" in
    *"printed 40 lines (budget 5)"*) ok "the breach names the count" ;;
    *) fail "the breach names the count: $out" ;;
esac
[ "$(wc -l < "$REPO/tmp/logs/ob-loud.log" | tr -d ' ')" = 40 ] \
    && ok "the log keeps every line the run printed" \
    || fail "the log keeps every line the run printed"

# Under budget: one verdict line naming the log, and the output NOT on the
# terminal.
out="$(run_tier ob-quiet ob-quiet 5 5000 -- echo hello)"
rc=$?
[ "$rc" = 0 ] && ok "a tier inside its budget exits 0" \
              || fail "a tier inside its budget exits 0 (got $rc)"
case "$out" in
    *hello*) fail "a passing tier keeps its output off the terminal: $out" ;;
    *) ok "a passing tier keeps its output off the terminal" ;;
esac
case "$out" in
    *"ob-quiet: ok"*"tmp/logs/ob-quiet.log"*) ok "a passing tier prints a verdict naming its log" ;;
    *) fail "a passing tier prints a verdict naming its log: $out" ;;
esac

# Failed: ONE LINE naming the log, and the COMMAND's own status, not the
# wrapper's. A failing tier is quiet too from core v0.2.13 -- the transcript is
# in the log and printing its tail costs every later reader for lines that are
# rarely where the assertion is.
out="$(run_tier ob-bad ob-bad 50 5000 -- sh -c 'echo the reason; exit 7')"
rc=$?
[ "$rc" = 7 ] && ok "a failing tier exits with the command's own status" \
              || fail "a failing tier exits with the command's own status (got $rc)"
case "$out" in
    *"ob-bad: FAILED (exit 7)"*"tmp/logs/ob-bad.log"*)
        ok "a failing tier names its status and its log" ;;
    *) fail "a failing tier names its status and its log: $out" ;;
esac
case "$out" in
    *"the reason"*) fail "a failing tier prints no tail unless asked: $out" ;;
    *) ok "a failing tier prints no tail unless asked" ;;
esac

# ...and the tail is still available to a person at a terminal.
out="$(OUTPUT_BUDGET_FAIL_TAIL=5 run_tier ob-tail ob-tail 50 5000 -- sh -c 'echo the reason; exit 7')"
case "$out" in
    *"the reason"*) ok "OUTPUT_BUDGET_FAIL_TAIL=N prints the tail on request" ;;
    *) fail "OUTPUT_BUDGET_FAIL_TAIL=N prints the tail on request: $out" ;;
esac

# Verbose streams, and is still budgeted. OUTPUT_BUDGET_VERBOSE is the name
# the canonical wrapper reads; the old FLTH_VERBOSE fails silently, which is
# why it is asserted rather than assumed.
out="$(OUTPUT_BUDGET_VERBOSE=1 run_tier ob-v ob-v 50 5000 -- echo hello)"
case "$out" in
    *hello*) ok "OUTPUT_BUDGET_VERBOSE=1 streams the run" ;;
    *) fail "OUTPUT_BUDGET_VERBOSE=1 streams the run: $out" ;;
esac

# --- 4. The resolver REFUSES, rather than falling back. -----------------
#
# FS_CORE_ROOT exists for these two cases and only for them: an explicit,
# authoritative root, so a refusal can be driven without moving the sibling
# checkout that every crate on the machine shares.
empty="$work/no-core"
mkdir -p "$empty"
out="$(FS_CORE_ROOT="$empty" run_tier ob-none ob-none 50 5000 -- echo x)"
rc=$?
[ "$rc" = 1 ] && ok "a core with no wrapper is refused (exit 1)" \
              || fail "a core with no wrapper is refused (exit 1, got $rc)"
case "$out" in
    *"rust-fs-core-output-budget 1"*) ok "the refusal names the expected API string" ;;
    *) fail "the refusal names the expected API string: $out" ;;
esac
case "$out" in
    *"v0.2.13"*) ok "the refusal names the minimum core version" ;;
    *) fail "the refusal names the minimum core version: $out" ;;
esac
case "$out" in
    *"rust-fs-core/scripts/output-budget.sh"*) ok "the refusal names the sibling path it looked for" ;;
    *) fail "the refusal names the sibling path it looked for: $out" ;;
esac

# A present-but-wrong wrapper is fatal. If this ever passed, a tier could run
# against an unknown script while reporting green.
wrong="$work/wrong-core"
mkdir -p "$wrong/scripts"
printf '#!/usr/bin/env bash\necho "rust-fs-core-output-budget 99"\n' > "$wrong/scripts/output-budget.sh"
out="$(FS_CORE_ROOT="$wrong" run_tier ob-wrong ob-wrong 50 5000 -- echo x)"
rc=$?
[ "$rc" = 1 ] && ok "a wrapper at the wrong API version is refused (exit 1)" \
              || fail "a wrapper at the wrong API version is refused (exit 1, got $rc)"
case "$out" in
    *"answered 'rust-fs-core-output-budget 99'"*) ok "the refusal quotes what it got" ;;
    *) fail "the refusal quotes what it got: $out" ;;
esac
# It must NOT have fallen through to the good sibling and run the command.
case "$out" in
    *"ob-wrong: ok"*) fail "a wrong wrapper does not fall back to a working one" ;;
    *) ok "a wrong wrapper does not fall back to a working one" ;;
esac

rm -f "$REPO/tmp/logs/ob-loud.log" "$REPO/tmp/logs/ob-quiet.log" \
      "$REPO/tmp/logs/ob-bad.log" "$REPO/tmp/logs/ob-tail.log" \
      "$REPO/tmp/logs/ob-v.log"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails output-budget violation(s)" >&2
    exit 1
fi
echo "PASS  every tier is budgeted, the wrapper comes from rust-fs-core, and a breached budget fails the run"
