#!/usr/bin/env bash
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HELPER=tests/support/src/kernel.rs
fails=0
ok() { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

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
    matches="$(git -C "$repository" grep -n -F 'check-pool.sh' -- \
        src tests scripts examples .github chores.yml Cargo.toml \
        ':(exclude)tests/scripts/pool-check-lock.sh')" || scan_status=$?
    if [ "$scan_status" -ne 0 ]; then
        echo "the tracked-source pool-check scan failed (status $scan_status)" >&2
        return 1
    fi
    while IFS= read -r match; do
        case "$match" in
            "$HELPER:$allowed_line:"*) ;;
            *)
                echo "check-pool.sh bypasses guest_kernel_pool_check: $match" >&2
                invalid=1
                ;;
        esac
    done <<< "$matches"
    return "$invalid"
}

if check_callers "$REPO"; then
    ok "only guest_kernel_pool_check references the pool-check script"
else
    fail "a tracked source bypasses the kernel registry lock"
fi

sandbox="$(mktemp -d)"
trap 'rm -r "$sandbox"' EXIT
for source in src/direct.rs scripts/direct.sh tests/support/src/kernel.rs; do
    planted="$sandbox/$(basename "$source")"
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

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails pool-check lock violation(s)" >&2
    exit 1
fi
echo "PASS  pool checks use the kernel registry lock, and direct-call mutations fail"
