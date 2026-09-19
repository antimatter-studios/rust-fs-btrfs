#!/usr/bin/env bash
#
# test-floor.sh — pass a test run's output through, and refuse a run that
# executed fewer tests than it is supposed to.
#
# Three ways to use it:
#
#   <command> | bash scripts/test-floor.sh 6
#       the run's output on stdin, passed through unchanged, and the
#       total refused if it is below the floor
#
#   bash scripts/test-floor.sh --log tmp/logs/oracle.log 120
#       the same, against a tier log a chore task already wrote. Every
#       job runs chore tasks now, and a chore task's output is a verdict
#       plus a log (see the budget table in chores.yml) -- so there is
#       nothing on the step's stdout left to count, and the numbers live
#       where the whole run does.
#
#   bash scripts/test-floor.sh --targets .github/test-floors.txt --log tmp/logs/suite.log
#       PER-TARGET floors: one number per integration target, from the
#       file, checked against that target's own `test result:` line. The
#       total is the weaker check -- a suite that empties from the inside
#       hides inside a total that other suites kept above the floor --
#       and this is what the old workflow's thirty-odd `cargo test --test
#       <name> | test-floor.sh <n>` steps were really for. Keeping it
#       means keeping that guard while the workflow stops naming suites
#       by hand, which is what let two of them run nowhere at all (#70).
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

# --targets FLOORS --log LOG: one floor per integration target.
if [ "${1:-}" = "--targets" ]; then
    floors="${2:-}"
    [ "${3:-}" = "--log" ] || {
        echo "test-floor.sh: usage: --targets <floors-file> --log <log>" >&2
        exit 2
    }
    log="${4:-}"
    [ -f "$floors" ] || { echo "test-floor.sh: no floors file at $floors" >&2; exit 2; }
    [ -f "$log" ] || {
        echo "test-floor.sh: no log at $log -- the tier that writes it did not run." >&2
        exit 1
    }

    # cargo prints `Running tests/<name>.rs (target/.../deps/<name>-<hash>)`
    # or `Running unittests src/lib.rs (...)` before each binary, and
    # libtest prints `test result: ok. N passed` after it. Pairing the two
    # is what gives a count per target.
    #
    # THE ESCAPE SEQUENCES COME OFF FIRST, and that is not tidiness.
    # Every job here sets CARGO_TERM_COLOR=always, so cargo colours its
    # own `Running` line -- libtest does not colour its summary, because
    # a pipe is not a terminal. So the totals matched and every PER-TARGET
    # count came back zero: fifty targets, each reported as a suite that
    # had emptied from the inside, on a run where all 500 tests passed.
    # A guard that reads a log must not depend on how the log was coloured.
    counts=$(awk '
        { gsub(/\033\[[0-9;]*[a-zA-Z]/, "") }
        /^[[:space:]]*Running unittests/ { target = "lib"; next }
        /^[[:space:]]*Running tests\// {
            target = $2
            sub(/^tests\//, "", target)
            sub(/\.rs$/, "", target)
            next
        }
        /^test result: ok\. [0-9]+ passed/ { if (target != "") seen[target] += $4 }
        END { for (t in seen) printf "%s %d\n", t, seen[t] }
    ' "$log")

    bad=0
    checked=0
    while read -r target floor; do
        case "$target" in ''|\#*) continue ;; esac
        case "$floor" in '' | *[!0-9]*)
            echo "test-floor.sh: $floors: '$target' has a non-numeric floor '${floor:-(none)}'" >&2
            exit 2
            ;;
        esac
        executed=$(echo "$counts" | awk -v t="$target" '$1 == t { print $2 }')
        executed=${executed:-0}
        checked=$((checked + 1))
        if [ "$executed" -lt "$floor" ]; then
            echo "::error::$target executed $executed tests, floor is $floor -- a target that executes fewer than its floor emptied from the inside rather than passed. If it legitimately shrank, lower its floor in $floors in the same commit." >&2
            bad=1
        fi
    done < "$floors"

    [ "$checked" -gt 0 ] || {
        echo "::error::$floors named no targets, so this checked nothing" >&2
        exit 1
    }
    echo "per-target floors: $checked target(s) checked against $log"
    exit "$bad"
fi

# --log LOG FLOOR: the total, from a log a chore task wrote.
if [ "${1:-}" = "--log" ]; then
    log="${2:-}"
    [ -f "$log" ] || {
        echo "test-floor.sh: no log at $log -- the tier that writes it did not run." >&2
        exit 1
    }
    floor="${3:-}"
    case "$floor" in
        '' | *[!0-9]*)
            echo "test-floor.sh: usage: --log <log> <floor>" >&2
            exit 2
            ;;
    esac
    executed=$(awk '
        { gsub(/\033\[[0-9;]*[a-zA-Z]/, "") }
        /test result: ok\. [0-9]+ passed/ { s += $4 }
        END { print s + 0 }
    ' "$log")
    echo "tests executed: $executed (floor $floor, from $log)"
    if [ "$executed" -lt "$floor" ]; then
        echo "::error::only $executed tests executed, floor is $floor -- a run that executes fewer tests than the floor stopped early rather than passed. If the suite legitimately shrank, lower the floor in .github/workflows/ci.yml in the same commit that shrank it, so the number stays something someone decided." >&2
        exit 1
    fi
    exit 0
fi

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
executed=$(awk '
    { gsub(/\033\[[0-9;]*[a-zA-Z]/, "") }
    /test result: ok\. [0-9]+ passed/ { s += $4 }
    END { print s + 0 }
' "$log")
echo "tests executed: $executed (floor $floor)"

if [ "$executed" -lt "$floor" ]; then
    echo "::error::only $executed tests executed, floor is $floor — a run that executes fewer tests than the floor stopped early rather than passed. If the suite legitimately shrank, lower the floor in .github/workflows/ci.yml in the same commit that shrank it, so the number stays something someone decided." >&2
    exit 1
fi
