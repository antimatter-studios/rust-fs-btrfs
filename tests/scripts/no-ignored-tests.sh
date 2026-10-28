#!/usr/bin/env bash
#
# no-ignored-tests.sh — A TIER THAT IGNORED A TEST FAILS IN CI.
#
# libtest prints `N ignored` and exits 0, so a tier can report green with
# tests that never ran. Full CI once passed all nine jobs on a run whose
# doc-tests ended `0 passed; 0 failed; 2 ignored` (#371): two examples in
# src/fs.rs carried `ignore` fences, and the count was printed and read by
# nothing. Nothing skips here, so the count is a gate:
#
#   * every `test-floor.sh TIER FLOOR` count in ci.yml and chores.yml is
#     taken with `--refuse-ignored`, which fails the tier on any ignored
#     test -- the doc-tests included, since they run in the suite and vm
#     tiers' logs;
#   * no documentation example in src/ is fenced `ignore`, the form that
#     put those two out of the run;
#   * the flag is proven to refuse before it is trusted: rust-fs-core's
#     test-floor.sh is run on a log reporting ignored tests and must fail,
#     and on the same log with none must pass.
#
#   bash tests/scripts/no-ignored-tests.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CORE="$REPO/../rust-fs-core"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

[ -f "$CORE/scripts/test-floor.sh" ] \
    || { echo "not ok ../rust-fs-core/scripts/test-floor.sh is missing (chore siblings:sync provides it)" >&2; exit 1; }

# Every count of the TIER FLOOR form; `--targets` is the per-target form,
# which counts passes only and is paired with a total of the same tier.
unrefused=""
counted=0
for f in .github/workflows/ci.yml chores.yml; do
    while IFS= read -r line; do
        counted=$(( counted + 1 ))
        case "$line" in
            *"test-floor.sh --refuse-ignored "*) ;;
            *) unrefused="$unrefused
    $f: $(printf '%s' "$line" | sed 's/^[[:space:]]*//')" ;;
        esac
    done < <(grep -E 'rust-fs-core/scripts/test-floor\.sh ' "$REPO/$f" \
        | grep -vE '^[[:space:]]*#' | grep -v -- '--targets')
done
[ "$counted" -gt 0 ] && ok "the workflows take $counted tier counts" \
    || fail "no test-floor.sh count was found in ci.yml or chores.yml"
[ -z "$unrefused" ] && ok "every tier count refuses an ignored test" \
    || fail "tier counts that pass with ignored tests:$unrefused"

fenced="$(grep -rnE '^[[:space:]]*//[/!][[:space:]]*```([a-z_]+,)*ignore([,[:space:]]|$)' "$REPO/src" || true)"
[ -z "$fenced" ] && ok "no documentation example in src/ is fenced ignore" \
    || fail "documentation examples fenced ignore, which no tier runs:
$fenced"

work="$(mktemp -d "${TMPDIR:-/tmp}/no-ignored.XXXXXX")"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/tmp/logs"
write_log() {
    cat > "$work/tmp/logs/suite.log" <<EOF
   Doc-tests fs_btrfs

running 2 tests
test src/fs.rs - fs::Filesystem::pool_reader (line 1370) ... $1
test src/fs.rs - fs::Filesystem::pool_reader (line 1386) ... $1

test result: ok. $2 passed; 0 failed; $3 ignored; 0 measured; 0 filtered out; finished in 0.00s
EOF
}
floor() { FS_CORE_CALLER="$work" bash "$CORE/scripts/test-floor.sh" --refuse-ignored suite 0 >/dev/null 2>&1; }
write_log ignored 0 2
floor && fail "a tier that ignored 2 tests is refused" || ok "a tier that ignored 2 tests is refused"
write_log ok 2 0
floor && ok "the same tier with none ignored is accepted" || fail "the same tier with none ignored is accepted"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails ignored-test gate violation(s)" >&2
    exit 1
fi
echo "PASS  a tier that ignored a test fails in CI"
