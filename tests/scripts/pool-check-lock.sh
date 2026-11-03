#!/usr/bin/env bash
#
# pool-check-lock.sh -- nothing runs tests/support/check-pool.sh except
# `guest_kernel_pool_check`, which holds the kernel registry lock (#757).
#
# A scan of the text cannot tell in general whether a line that names the script
# runs it, so a guess at what running looks like would pass whatever it did not
# think of. The rule fails closed instead: every line that names the script is a
# bypass, except the helper's own line and the exact lines reads_the_text lists.
#
# It cannot see a name it is not given: a path spelled in pieces or by a glob, or
# a function handed the path that then runs it. This file excludes itself, because
# its own mutations have to spell the direct call.
#
#   bash tests/scripts/pool-check-lock.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HELPER=tests/support/src/kernel.rs
READER=tests/scripts/kernel-helper-forget.sh
fails=0
ok() { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# reads_the_text <line>: whether a line of $READER is one that may name the
# script. That guard reads the script to check its settle, forget and attach
# order (#754, #759), and never runs it. It holds the path in CHECK_POOL, so the
# scan follows that name as well, and the only use allowed is the one that hands
# the path to order_of. Anything else in that file is refused like any other, and
# the same lines in any other file are refused because the rule names this file.
reads_the_text() {
    case "$1" in
        '# `guest_kernel_read_variants`, `pool_run`, and tests/support/check-pool.sh.' | \
        'CHECK_POOL="$REPO/tests/support/check-pool.sh"' | \
        'verdict="$(order_of "$CHECK_POOL")"' | \
        '    && ok "check-pool.sh settles, forgets, then attaches" \' | \
        '    || fail "check-pool.sh settles, forgets, then attaches: $verdict"')
            return 0
            ;;
    esac
    return 1
}

check_callers() {
    local repository="$1" allowed_line matches match scan_status=0 invalid=0
    allowed_line="$(awk '
        /^pub fn guest_kernel_pool_check\(/ { inside = 1 }
        inside && /check-pool[.]sh/ { print FNR }
        inside && /^}/ { inside = 0 }
    ' "$repository/$HELPER")" || return 1
    if [[ ! "$allowed_line" =~ ^[0-9]+$ ]]; then
        echo "guest_kernel_pool_check must contain exactly one pool-check script reference" >&2
        return 1
    fi
    matches="$(git -C "$repository" grep -n -E 'check-pool[.]sh|CHECK_POOL' -- \
        src tests scripts examples .github chores.yml Cargo.toml \
        ':(exclude)tests/scripts/pool-check-lock.sh')" || scan_status=$?
    if [ "$scan_status" -ne 0 ]; then
        echo "the tracked-source pool-check scan failed (status $scan_status)" >&2
        return 1
    fi
    while IFS= read -r match; do
        case "$match" in
            "$HELPER:$allowed_line:"*) continue ;;
            "$READER:"*) reads_the_text "${match#*:*:}" && continue ;;
        esac
        echo "check-pool.sh bypasses guest_kernel_pool_check: $match" >&2
        invalid=1
    done <<< "$matches"
    return "$invalid"
}

if check_callers "$REPO"; then
    ok "only guest_kernel_pool_check runs the pool-check script"
else
    fail "a tracked source bypasses the kernel registry lock"
fi

sandbox="$(mktemp -d)"
# A git object is read-only, and on a terminal rm stops to ask about each one.
trap 'chmod -R u+w "$sandbox"; rm -r "$sandbox"' EXIT
for source in src/direct.rs scripts/direct.sh tests/support/src/kernel.rs tests/direct.rs tests/scripts/direct.sh; do
    planted="$sandbox/${source//\//-}"
    mkdir -p "$planted/tests/support/src" "$planted/$(dirname "$source")"
    cp "$REPO/$HELPER" "$planted/$HELPER"
    git -C "$planted" init --quiet
    git -C "$planted" add "$HELPER"
    if ! check_callers "$planted" > "$sandbox/verdict" 2>&1; then
        fail "the helper-only tracked tree is accepted: $source"
        cat "$sandbox/verdict" >&2
    fi
    case "$source" in
        *.sh) printf '\nbash tests/support/check-pool.sh\n' >> "$planted/$source" ;;
        *) printf '\nfn bypass() { std::process::Command::new("bash").arg("tests/support/check-pool.sh"); }\n' >> "$planted/$source" ;;
    esac
    git -C "$planted" add "$source"
    if check_callers "$planted" > "$sandbox/verdict" 2>&1; then
        fail "a direct pool check is refused: $source"
    elif grep -q -F "check-pool.sh bypasses guest_kernel_pool_check: $source:" "$sandbox/verdict"; then
        ok "a direct pool check is refused: $source"
    else
        fail "the mutation failed for another reason: $source"
        cat "$sandbox/verdict" >&2
    fi
done

# plant_with_reader: a tracked tree holding the helper and the real $READER.
plant_with_reader() {
    planted="$(mktemp -d "$sandbox/reader.XXXXXX")"
    mkdir -p "$planted/$(dirname "$HELPER")" "$planted/$(dirname "$READER")"
    cp "$REPO/$HELPER" "$planted/$HELPER"
    cp "$REPO/$READER" "$planted/$READER"
    git -C "$planted" init --quiet
    git -C "$planted" add "$HELPER" "$READER"
}

# expect_refused <what> <file>: <file>, changed in the planted tree, is refused.
expect_refused() {
    git -C "$planted" add "$2"
    if check_callers "$planted" > "$sandbox/verdict" 2>&1; then
        fail "$1 is refused"
    elif grep -q -F "check-pool.sh bypasses guest_kernel_pool_check: $2:" "$sandbox/verdict"; then
        ok "$1 is refused"
    else
        fail "the mutation failed for another reason: $1"
        cat "$sandbox/verdict" >&2
    fi
}

plant_with_reader
if check_callers "$planted" > "$sandbox/verdict" 2>&1; then
    ok "the forget-order guard reading the script's text is accepted"
else
    fail "the forget-order guard reading the script's text is accepted"
    cat "$sandbox/verdict" >&2
fi

printf '\nbash tests/support/check-pool.sh\n' >> "$planted/$READER"
expect_refused "a direct run added to the forget-order guard" "$READER"

plant_with_reader
printf '\nbash "$CHECK_POOL"\n' >> "$planted/$READER"
expect_refused "the forget-order guard running its own variable" "$READER"

plant_with_reader
cp "$planted/$READER" "$planted/tests/scripts/copy.sh"
expect_refused "a new guard holding the lines only the forget-order guard may hold" tests/scripts/copy.sh

plant_with_reader
printf 'source tests/scripts/kernel-helper-forget.sh\nbash "$CHECK_POOL"\n' >> "$planted/tests/scripts/alias.sh"
expect_refused "a new guard running the forget-order guard's variable" tests/scripts/alias.sh

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails pool-check lock violation(s)" >&2
    exit 1
fi
echo "PASS  pool checks use the kernel registry lock, and direct-call mutations fail"
