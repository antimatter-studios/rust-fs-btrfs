#!/usr/bin/env bash
#
# executed-test-floors.sh — THE FLOOR COUNTS WHAT RAN, whatever colour
# the run was printed in.
#
# scripts/test-floor.sh is the guard that refuses a suite which emptied
# from the inside: `cargo test` exits 0 on "0 passed; 0 failed", so a
# target that selected nothing looks exactly like a target that passed
# everything. It works by pairing cargo's `Running tests/<name>.rs` line
# with libtest's `test result: ok. N passed` summary.
#
# WHY THIS FILE EXISTS. Every job in ci.yml sets CARGO_TERM_COLOR=always.
# Cargo colours its own `Running` line; libtest does not colour its
# summary, because a pipe is not a terminal. So on a green run of 500
# tests the per-target parse matched nothing and reported all fifty
# targets as zero — the guard failing closed, but for a reason that had
# nothing to do with the suite. A guard that reads a log must not depend
# on how the log was coloured, and that is what this holds it to.
#
#   bash tests/scripts/executed-test-floors.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FLOOR="$REPO/scripts/test-floor.sh"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

work="$(mktemp -d "${TMPDIR:-/tmp}/test-floor-check.XXXXXX")"
trap 'rm -rf "$work"' EXIT

esc=$(printf '\033')

# A log in each colouring, with the same content: two targets and the
# library, four tests between them.
plain="$work/plain.log"
cat > "$plain" <<'LOG'
     Running unittests src/lib.rs (target/release/deps/fs_btrfs-1111)

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running tests/alpha.rs (target/release/deps/alpha-2222)

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running tests/beta.rs (target/release/deps/beta-3333)

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
LOG

# The same log as CARGO_TERM_COLOR=always produces it: cargo's status
# word is bold green, the path is not, and libtest's summary is plain.
coloured="$work/coloured.log"
sed "s|     Running|     ${esc}[0m${esc}[0m${esc}[1m${esc}[32mRunning${esc}[0m|" "$plain" > "$coloured"

floors="$work/floors.txt"
cat > "$floors" <<'LOG'
# <target> <floor>
lib 2
alpha 1
beta 1
LOG

for which in plain coloured; do
    log="$work/$which.log"

    if bash "$FLOOR" --targets "$floors" --log "$log" >/dev/null 2>&1; then
        ok "per-target floors are met by a $which log"
    else
        fail "per-target floors are met by a $which log"
    fi

    got=$(bash "$FLOOR" --log "$log" 1 2>/dev/null | sed -n 's/^tests executed: \([0-9]*\) .*/\1/p')
    if [ "$got" = 4 ]; then
        ok "the total from a $which log is 4"
    else
        fail "the total from a $which log is 4 (got '${got:-nothing}')"
    fi
done

# And the guard still fails when a target really did empty out: a floor
# above what the log shows, on the coloured log, must be refused.
raised="$work/raised.txt"
printf 'alpha 9\n' > "$raised"
if bash "$FLOOR" --targets "$raised" --log "$work/coloured.log" >/dev/null 2>&1; then
    fail "a target below its floor is refused in a coloured log"
else
    ok "a target below its floor is refused in a coloured log"
fi

# A target absent from the log counts as zero rather than being skipped:
# a suite that stopped being BUILT is the case a name-based check misses.
printf 'gamma 1\n' > "$raised"
if bash "$FLOOR" --targets "$raised" --log "$work/coloured.log" >/dev/null 2>&1; then
    fail "a target missing from the log counts as zero"
else
    ok "a target missing from the log counts as zero"
fi

[ "$fails" = 0 ] || exit 1
