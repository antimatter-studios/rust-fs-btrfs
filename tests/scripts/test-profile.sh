#!/usr/bin/env bash
# Test binaries never use the LTO-heavy release profile used for artifacts.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok() { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$((fails + 1)); }

profile="$(sed -n '/^\[profile.test\]/,/^\[/p' "$REPO/Cargo.toml")"
printf '%s\n' "$profile" | rg -q '^lto = false$' &&
    ok 'the test profile disables LTO' || fail 'the test profile disables LTO'
printf '%s\n' "$profile" | rg -q '^opt-level = 1$' &&
    ok 'the test profile uses dev-level optimisation' || fail 'the test profile uses dev-level optimisation'
if printf '%s\n' "$profile" | rg -q '^codegen-units[[:space:]]*='; then
    fail "the test profile overrides Cargo's default codegen units"
else
    ok "the test profile keeps Cargo's default codegen units"
fi

if rg -n -- 'scripts/test\.sh .*--release|scripts/test\.sh .*--profile[ =]release' \
    "$REPO/chores.yml" "$REPO/scripts/guest-suite.sh" "$REPO/.github/workflows/ci.yml"; then
    fail 'a test tier still selects the release profile'
else
    ok 'no test tier selects the release profile'
fi

for args in '--release' '-r' '--profile=release' '--profile release'; do
    # Deliberate word splitting tests both spellings of --profile.
    # shellcheck disable=SC2086
    if output="$("$REPO/scripts/test.sh" $args 2>&1)"; then
        fail "test.sh accepted $args"
    elif [[ "$output" == *'release optimisations are for shipped artifacts'* ]]; then
        ok "test.sh refuses $args"
    else
        fail "test.sh refused $args without naming the release-profile rule"
    fi
done

[[ "$fails" -eq 0 ]]
