#!/usr/bin/env bash
# scripts/disk-watch.sh reports the lowest free space a command reached, which
# the free space before and after it cannot show, and keeps the command's status
# (#731). df is a stand-in here that reads its answer from a file, so the free
# space falls and recovers on the script's terms.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/disk-watch.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT
mkdir "$sandbox/bin"
export DISK_WATCH_FREE="$sandbox/free" DISK_WATCH_INTERVAL=0.1
cat >"$sandbox/bin/df" <<'EOF'
#!/usr/bin/env bash
echo "Filesystem 1048576-blocks Used Available Capacity Mounted on"
echo "/dev/stand-in 100000 0 $(cat "$DISK_WATCH_FREE") 0% /"
EOF
chmod +x "$sandbox/bin/df"

watch() { PATH="$sandbox/bin:$PATH" bash "$REPO/scripts/disk-watch.sh" "$@"; }

# A command that uses disk and gives it back: long enough to be sampled.
echo 7000 >"$DISK_WATCH_FREE"
out="$(watch oracle bash -c 'echo 500 >"$DISK_WATCH_FREE"; sleep 1; echo 6900 >"$DISK_WATCH_FREE"')"
status=$?
if [ "$status" -eq 0 ]; then ok "a passing command's status is kept"; else fail "status $status for a passing command"; fi
want="disk: oracle: 7000 MB free before, 500 MB at the lowest, 6900 MB after"
if [ "$(printf '%s\n' "$out" | tail -n 1)" = "$want" ]; then
    ok "the lowest free space is reported, not only the two ends"
else
    fail "expected '$want', got '$out'"
fi

# A failing command's status passes through, and it still gets its line.
echo 7000 >"$DISK_WATCH_FREE"
out="$(watch oracle bash -c 'exit 101')"
status=$?
if [ "$status" -eq 101 ]; then ok "a failing command's status is kept"; else fail "status $status for exit 101"; fi
case "$out" in
    *"disk: oracle: 7000 MB free before"*) ok "a failing command still gets its line" ;;
    *) fail "no line for a failing command: '$out'" ;;
esac

# Space lost right at the end, between samples, is still the lowest.
echo 7000 >"$DISK_WATCH_FREE"
out="$(DISK_WATCH_INTERVAL=30 watch oracle bash -c 'echo 12 >"$DISK_WATCH_FREE"')"
case "$out" in
    *"12 MB at the lowest, 12 MB after"*) ok "the free space at the end counts towards the lowest" ;;
    *) fail "the end was not counted: '$out'" ;;
esac

# A df that answers nothing is named as unknown, not reported as zero.
cat >"$sandbox/bin/df" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
out="$(watch oracle true)"
status=$?
if [ "$status" -eq 0 ] && [ "$out" = "disk: oracle: unknown MB free before, unknown MB at the lowest, unknown MB after" ]; then
    ok "a disk that cannot be asked about is unknown, and does not fail the command"
else
    fail "with no df: status $status, '$out'"
fi

# Without a command there is nothing to run.
watch oracle >/dev/null 2>&1
status=$?
if [ "$status" -eq 64 ]; then ok "a missing command is a usage error"; else fail "status $status with no command"; fi

[ "$fails" -eq 0 ] && echo "disk-watch.sh: all checks passed"
exit $(( fails > 0 ))
