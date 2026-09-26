#!/usr/bin/env bash
# tier.sh LABEL LOG-NAME MAX-LINES MAX-BYTES -- COMMAND [ARG...]
#
# One test tier, run QUIETLY and under a budget. The whole run goes to
# tmp/logs/<LOG-NAME>.log, and EVERY outcome is one line: a pass names the log
# and its size, a failure names the command's own status and the log holding
# the reason, and a run that passed but printed more than its budget fails
# with status 65.
#
# A FAILURE DOES NOT PRINT A TAIL, deliberately. `OUTPUT_BUDGET_FAIL_TAIL=N`
# asks for one when a person is at a terminal. The default is none, because
# the tail is rarely where the assertion is, and the reader who pays most --
# an agent re-reading its transcript on every later step -- pays for those
# lines many times over. One line naming the log lets it read the part it
# wants, once.
#
# WHY THE BUDGET IS PART OF THE TASK and not a CI-only check: the reader who
# pays most for a noisy suite is the one running it locally, and a rule that
# only CI enforces is a rule the tree drifts away from between pull requests.
#
# The budgets themselves are in chores.yml, next to the command each one
# bounds, and every one of them was MEASURED — see the table there. Raise one
# deliberately when a tier grows; a budget nobody can breach measures nothing.
#
# VERBOSE. `OUTPUT_BUDGET_VERBOSE=1`, or `--verbose`/`-v` in the chore
# invocation's CLI_ARGS (`chore test:oracle -- --verbose`), streams the run as
# it happens as well as logging it. It does NOT lift the budget: the log is
# the same size either way, and a tier that has outgrown its budget should say
# so whether or not anybody was watching.
#
# THE VARIABLE WAS `FLTH_VERBOSE` until the wrapper moved to rust-fs-core.
# A rename like that FAILS SILENTLY -- the old name is not read, nothing
# errors, and the run simply stays quiet -- which is why it is recorded here,
# where somebody grepping for the name they remember will land. The canonical
# script reports the old name being set rather than honouring it; this script
# never reads it at all. `FLTH_FAIL_TAIL` became `OUTPUT_BUDGET_FAIL_TAIL` the
# same way.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# THE WRAPPER COMES FROM rust-fs-core, AND IS COPIED FOR THIS RUN ONLY.
#
# It belongs to core, and it is deliberately NOT committed here. A committed
# copy is a copy that drifts: measured on 2026-09-22 the family had three of
# them, reached four different ways, each repository internally consistent and
# nothing comparing them. It used to be read from
# ../fs-linux-test-harness/scripts/output-budget.sh, which made a harness the
# owner of a rule that has nothing to do with VMs.
#
# THE ORDER, and why it is this way round:
#
#   1. FS_CORE_ROOT, if set. An explicit root, and authoritative: nothing
#      falls back past it. It exists so tests/scripts/output-budget.sh can
#      drive the refusal cases, which is the only way to prove the refusals
#      are real.
#   2. ../rust-fs-core — THE SIBLING FIRST. A coordinated change to the
#      wrapper is made in the sibling checkout, so the sibling is what has to
#      be exercised; resolving the packaged copy first would run the pinned
#      release and report green on a change nobody ran.
#   3. cargo metadata for the am-fs-core package root, so a checkout with no
#      sibling beside it — a `cargo install`-shaped tree, or a registry
#      dependency — still resolves. `scripts/` is inside the published
#      .crate: core's Cargo.toml excludes only `fuzz`.
#
# THE CONTRACT IS `--version`, NOT A DIGEST. Whatever is found must answer
# `--version` with exactly `rust-fs-core-output-budget 1`, and a copy that
# answers anything else is FATAL — not a reason to try the next candidate,
# because falling back would hide the very change being tested. It is
# deliberately not a SHA-256 the way rust-fs-ntfs pins one: a digest recorded
# in seven repositories has to be updated in seven repositories for any edit
# to core, which recreates the lockstep this migration exists to remove. The
# API version is the thing that changes when the contract changes.
#
# tmp/ is gitignored and is where the tier logs already live.
OUTPUT_BUDGET_API="rust-fs-core-output-budget 1"
SIBLING="$REPO/../rust-fs-core"

core_wrapper() {
    if [ -n "${FS_CORE_ROOT:-}" ]; then
        printf '%s\n' "$FS_CORE_ROOT/scripts/output-budget.sh"
        return
    fi
    if [ -f "$SIBLING/scripts/output-budget.sh" ]; then
        printf '%s\n' "$SIBLING/scripts/output-budget.sh"
        return
    fi
    cargo metadata --format-version 1 --locked --manifest-path "$REPO/Cargo.toml" \
        2>/dev/null | python3 -c '
import json, sys
try:
    packages = json.load(sys.stdin)["packages"]
except Exception:
    sys.exit(0)
root = next((p["manifest_path"].rsplit("/", 1)[0]
             for p in packages if p["name"] == "am-fs-core"), "")
if root:
    print(root + "/scripts/output-budget.sh")
'
}

refuse() {
    echo "tier.sh: no usable output-budget wrapper from rust-fs-core." >&2
    echo "         $1" >&2
    echo "         Looked for: $SIBLING/scripts/output-budget.sh" >&2
    echo "         then the am-fs-core package root cargo resolves." >&2
    echo "         It must answer --version with exactly: $OUTPUT_BUDGET_API" >&2
    echo "         This repository needs rust-fs-core v0.2.13 or later;" >&2
    echo "         'chore siblings' checks the sibling out at the pinned ref." >&2
    exit 1
}

WRAPPER="$(core_wrapper)"
if [ -z "$WRAPPER" ]; then
    refuse "Nothing resolved: no sibling, and cargo named no am-fs-core root."
elif [ ! -f "$WRAPPER" ]; then
    refuse "$WRAPPER does not exist."
fi

# A PRESENT-BUT-WRONG COPY IS FATAL. Not a fall-through: a wrapper that
# answers the wrong version is the interesting failure, and quietly using a
# different one would turn it into a pass.
GOT="$(bash "$WRAPPER" --version 2>/dev/null || true)"
[ "$GOT" = "$OUTPUT_BUDGET_API" ] || \
    refuse "$WRAPPER answered '$GOT'."

BUDGET="$REPO/tmp/output-budget.$$.sh"
mkdir -p "$REPO/tmp"
cp "$WRAPPER" "$BUDGET"
trap 'rm -f "$BUDGET"' EXIT

[ $# -ge 5 ] || { echo "tier.sh: usage: tier.sh LABEL LOG MAX-LINES MAX-BYTES -- CMD..." >&2; exit 2; }
LABEL="$1"; LOG_NAME="$2"; MAX_LINES="$3"; MAX_BYTES="$4"; shift 4
[ "${1:-}" = "--" ] && shift
[ $# -gt 0 ] || { echo "tier.sh: no command" >&2; exit 2; }

# `chore test:oracle -- --verbose` arrives as CLI_ARGS. output-budget.sh reads
# OUTPUT_BUDGET_VERBOSE itself, so mapping the flag onto it is all that is
# needed -- and it means the environment variable and the flag cannot disagree.
case " ${CLI_ARGS:-} " in
    *" --verbose "*|*" -v "*) export OUTPUT_BUDGET_VERBOSE=1 ;;
esac

bash "$BUDGET" \
    --log "$REPO/tmp/logs/$LOG_NAME.log" \
    --max-lines "$MAX_LINES" \
    --max-bytes "$MAX_BYTES" \
    --label "$LABEL" \
    -- "$@"
