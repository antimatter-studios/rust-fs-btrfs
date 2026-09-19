#!/usr/bin/env bash
#
# output-budget.sh — THE OUTPUT RULE, CHECKED: every test tier is
# budgeted, and a budget can actually fail a run.
#
# Quiet-by-default is a convention, and a convention rots in a week. This
# is the number that fails the build instead: a tier added later without
# going through scripts/tier.sh, or given a budget of zero (which
# output-budget.sh reads as "no budget"), fails here rather than being
# noticed the next time somebody scrolls past three thousand lines.
#
# See the fs-linux-test-harness README, "Output: quiet by default,
# --verbose on request", and the measured budget table in chores.yml.
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

# --- 2. A budget that is breached fails the run. -------------------------
budget="$REPO/../fs-linux-test-harness/scripts/output-budget.sh"
if [ ! -x "$budget" ]; then
    fail "../fs-linux-test-harness/scripts/output-budget.sh is present (run 'chore siblings')"
else
    mkdir -p "$REPO/tmp"
    work="$(mktemp -d "$REPO/tmp/output-budget-test.XXXXXX")"
    trap 'rm -rf "$work"' EXIT

    # Passed, but printed too much: status 65, distinct from a failing suite.
    out="$("$budget" --log "$work/loud.log" --max-lines 5 --label loud \
              -- sh -c 'i=0; while [ $i -lt 40 ]; do echo line $i; i=$((i+1)); done' 2>&1)"
    rc=$?
    [ "$rc" = 65 ] && ok "a run over its line budget exits 65" \
                   || fail "a run over its line budget exits 65 (got $rc)"
    case "$out" in
        *"printed 40 lines (budget 5)"*) ok "the breach names the count" ;;
        *) fail "the breach names the count: $out" ;;
    esac
    [ "$(wc -l < "$work/loud.log" | tr -d ' ')" = 40 ] \
        && ok "the log keeps every line the run printed" \
        || fail "the log keeps every line the run printed"

    # Under budget: one verdict line, and the output is NOT on the terminal.
    out="$("$budget" --log "$work/quiet.log" --max-lines 5 --label quiet -- echo hello 2>&1)"
    rc=$?
    [ "$rc" = 0 ] && ok "a run inside its budget exits 0" \
                  || fail "a run inside its budget exits 0 (got $rc)"
    case "$out" in
        *hello*) fail "a passing run keeps its output off the terminal: $out" ;;
        *) ok "a passing run keeps its output off the terminal" ;;
    esac
    case "$out" in
        *"quiet: ok"*) ok "a passing run prints a verdict" ;;
        *) fail "a passing run prints a verdict: $out" ;;
    esac

    # Failed: the excerpt, and the command's own status.
    out="$("$budget" --log "$work/bad.log" --max-lines 5 --tail 3 --label bad \
              -- sh -c 'echo the reason; exit 7' 2>&1)"
    rc=$?
    [ "$rc" = 7 ] && ok "a failing run exits with the command's own status" \
                  || fail "a failing run exits with the command's own status (got $rc)"
    case "$out" in
        *"the reason"*) ok "a failure prints the excerpt" ;;
        *) fail "a failure prints the excerpt: $out" ;;
    esac

    # Verbose streams, and is still budgeted.
    out="$(FLTH_VERBOSE=1 "$budget" --log "$work/v.log" --max-lines 5 --label v -- echo hello 2>&1)"
    case "$out" in
        *hello*) ok "--verbose streams the run" ;;
        *) fail "--verbose streams the run: $out" ;;
    esac
fi

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails output-budget violation(s)" >&2
    exit 1
fi
echo "PASS  every test tier is budgeted, and a breached budget fails the run"
