#!/usr/bin/env bash
#
# test-floor.sh — pass a test run's output through, and refuse a run that
# executed fewer tests than it is supposed to.
#
# Usage, from a workflow step:
#
#   cargo test --test csum_oracle -- --nocapture | bash scripts/test-floor.sh 6
#
# A FLOOR, BECAUSE THE FAILURE MODE HERE IS AN ABSENCE.
#
# `cargo test` exits 0 on "0 passed; 0 failed", so a target that selected
# nothing looks exactly like a target that passed everything. The
# kernel-gate job is built out of thirty-four `cargo test --test <name>`
# invocations, each naming ONE integration target, and a name is the
# weakest link a build can hang from: rename `tests/csum_oracle.rs` and
# the step that runs it does not fail — cargo says "no test target named
# csum_oracle"... only if the name is wrong at the cargo level, and a
# suite emptied from the inside (every test renamed, feature-gated out,
# or moved to another file) says nothing at all. It reports `0 passed`,
# green, and the oracle that is the whole reason this repository can
# claim to read Btrfs stops running with no one told.
#
# That is what this refuses. Every invocation carries the number of tests
# it executed on a known-good run, and a run that executes fewer stopped
# early rather than passed. The number moves UP with the suite and never
# down: raising it when tests are added is bookkeeping, lowering it is a
# decision someone has to write down in a diff.
#
# Counting rather than naming is deliberate — a list of expected test
# names is a second copy of the suite to maintain, and the thing that
# goes wrong is the count going to zero, not a particular test
# disappearing.
#
# The output is passed through unchanged, so `--nocapture` still prints
# what it prints and the step's log reads as it did before.
#
# Exit status: this script only ever reports the floor. The exit status
# of `cargo test` itself is preserved by `pipefail`, which every job here
# gets from `defaults: run: shell: bash` — a real test failure still
# fails the step whatever the count says.
set -uo pipefail

floor=${1:-}
case "$floor" in
    '' | *[!0-9]*)
        echo "test-floor.sh: usage: <command> | bash scripts/test-floor.sh <floor>" >&2
        echo "test-floor.sh: refusing a non-numeric floor ${floor:-(none)}" >&2
        exit 2
        ;;
esac

log=$(mktemp)
trap 'rm -f "$log"' EXIT

# `tee` is what keeps the run's own output on the step's log. Without it
# a failing oracle would print its explanation into this script and
# nowhere a human looks.
tee "$log"

# `test result: ok. N passed` is libtest's own summary line, one per test
# binary, and summing them covers a run that built several. `awk` rather
# than `bc` so this works unchanged on a macOS or Windows runner if a
# matrix leg is ever added.
executed=$(grep -aoE 'test result: ok\. [0-9]+ passed' "$log" | awk '{s+=$4} END{print s+0}')
echo "tests executed: $executed (floor $floor)"

if [ "$executed" -lt "$floor" ]; then
    echo "::error::only $executed tests executed, floor is $floor — a run that executes fewer tests than the floor stopped early rather than passed. If the suite legitimately shrank, lower the floor in .github/workflows/ci.yml in the same commit that shrank it, so the number stays something someone decided." >&2
    exit 1
fi
