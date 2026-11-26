#!/usr/bin/env bash
# scripts/run-bounded.sh stops a command that does not end, names it, stops what
# it started, and otherwise keeps the command's status (#813). The script tier
# runs every guard under it, so a guard that stops making progress fails the job
# by name rather than holding it until the job's time limit.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/run-bounded.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT
bounded() { bash "$REPO/scripts/run-bounded.sh" "$@"; }

out="$(bounded 30 bash -c 'echo said; exit 101' 2>&1)"
status=$?
if [ "$status" -eq 101 ] && [[ "$out" == *said* ]]; then
    ok "a command that ends keeps its status and its output"
else
    fail "a command that ends: status $status, '$out'"
fi

# A command that never ends, with a child of its own that never ends either.
cat > "$sandbox/blocked.sh" <<'BLOCKED'
#!/usr/bin/env bash
sleep 300 &
echo $! > "$1"
wait
BLOCKED
started=$SECONDS
out="$(bounded 2 bash "$sandbox/blocked.sh" "$sandbox/child" 2>&1)"
status=$?
took=$((SECONDS - started))
if [ "$status" -eq 124 ] && [ "$took" -lt 60 ]; then
    ok "a blocked command is stopped at its bound"
else
    fail "a blocked command: status $status after ${took}s"
fi
case "$out" in
    *"still running after 2s and stopped: bash $sandbox/blocked.sh"*) ok "and is named" ;;
    *) fail "the stopped command is not named: '$out'" ;;
esac
child="$(cat "$sandbox/child" 2>/dev/null || true)"
if [ -n "$child" ] && ! kill -0 "$child" 2>/dev/null; then
    ok "and what it started is stopped with it"
else
    fail "its child is still running (pid ${child:-never written})"
    [ -z "$child" ] || kill -KILL "$child" 2>/dev/null
fi

# The tier uses it: every guard, one bound each.
# shellcheck disable=SC2016  # the manifest's own text, `$t` and all
if grep -qF 'bash scripts/run-bounded.sh 900 bash "$t"' "$REPO/chores.yml"; then
    ok "the script tier runs each guard under a bound"
else
    fail "chores.yml's test:scripts no longer runs each guard under scripts/run-bounded.sh"
fi

[ "$fails" -eq 0 ]
