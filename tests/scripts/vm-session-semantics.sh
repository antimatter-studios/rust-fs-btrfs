#!/usr/bin/env bash
#
# vm-session-semantics.sh — what vm-session.sh does to a script's exit
# status.
#
# The rule is easy to state and easy to get backwards, and it was
# backwards once: a failing teardown fails an otherwise successful
# script. A machine that would not stop is the condition the trap exists
# to prevent, so reporting success while it is still running defeats the
# whole thing.
#
# `vm.sh` is stubbed, so this asserts on the exit status the trap
# produces rather than on a VM actually halting.
#
#   bash tests/scripts/vm-session-semantics.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0

# A throwaway tree holding a stub vm.sh next to the real vm-session.sh,
# so sourcing resolves to the stub.
sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
mkdir -p "$sandbox/scripts"
cp "$REPO/scripts/vm-session.sh" "$sandbox/scripts/"

# $1 = exit status the stubbed `vm.sh down` returns
# $2 = exit status of the script body
# $3 = expected final status
# $4 = what is being asserted
check() {
    local down=$1 body=$2 want=$3 what=$4
    cat > "$sandbox/scripts/vm.sh" <<EOF
#!/usr/bin/env bash
[ "\${1:-}" = "down" ] || exit 0
exit $down
EOF
    chmod +x "$sandbox/scripts/vm.sh"

    cat > "$sandbox/scripts/work.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
source "\$(dirname "\${BASH_SOURCE[0]}")/vm-session.sh"
exit $body
EOF
    chmod +x "$sandbox/scripts/work.sh"

    local got=0
    "$sandbox/scripts/work.sh" >/dev/null 2>&1 || got=$?
    if [ "$got" -eq "$want" ]; then
        printf 'ok    %s\n' "$what"
    else
        printf 'FAIL  %s (expected %s, got %s)\n' "$what" "$want" "$got"
        fails=$((fails + 1))
    fi
}

check 0 0 0 "work succeeds, teardown succeeds"
check 0 3 3 "work fails, teardown succeeds: the work's status survives"
# The one that matters, and the one that was wrong.
check 1 0 1 "work succeeds, teardown FAILS: the script fails"
check 1 3 3 "both fail: the earlier, more informative failure wins"

# KEEP_VM leaves the machine up and does not consult teardown at all, so
# a stub that would fail must not affect the status.
cat > "$sandbox/scripts/vm.sh" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = "down" ] && { echo "down was called" >&2; exit 1; }
exit 0
EOF
chmod +x "$sandbox/scripts/vm.sh"
cat > "$sandbox/scripts/work.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"
exit 0
EOF
chmod +x "$sandbox/scripts/work.sh"
out="$(KEEP_VM=1 "$sandbox/scripts/work.sh" 2>&1)"; got=$?
if [ "$got" -eq 0 ] && [[ "$out" != *"down was called"* ]]; then
    printf 'ok    KEEP_VM=1 skips teardown entirely\n'
else
    printf 'FAIL  KEEP_VM=1 should not call down (status %s, output: %s)\n' "$got" "$out"
    fails=$((fails + 1))
fi

# The trap must fire on an abort, not only on a clean exit -- a wrapper
# dying under `set -e` is the common case.
cat > "$sandbox/scripts/vm.sh" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = "down" ] && echo "TEARDOWN RAN"
exit 0
EOF
chmod +x "$sandbox/scripts/vm.sh"
cat > "$sandbox/scripts/work.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"
false          # aborts under set -e
echo "not reached"
EOF
chmod +x "$sandbox/scripts/work.sh"
out="$("$sandbox/scripts/work.sh" 2>&1)" || true
if [[ "$out" == *"TEARDOWN RAN"* ]]; then
    printf 'ok    teardown runs when the body aborts under set -e\n'
else
    printf 'FAIL  teardown did not run on an abort (output: %s)\n' "$out"
    fails=$((fails + 1))
fi

if [ "$fails" -eq 0 ]; then
    echo "PASS  vm-session exit semantics"
else
    echo "FAIL  $fails assertion(s)" >&2
    exit 1
fi
