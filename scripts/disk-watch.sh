#!/usr/bin/env bash
# disk-watch.sh LABEL COMMAND...  run COMMAND and say what it did to the free
# space on this directory's disk.
#
# A tier cleans its scratch up when it ends, so the free space before and after
# it are about the same whatever it used. What a reader of a failed job wants is
# the lowest it reached, so that is sampled while COMMAND runs. One line, on
# stdout, outside the tier's budgeted log:
#
#   disk: oracle: 72000 MB free before, 60000 MB at the lowest, 71900 MB after
#
# About one job in fifty lands on a runner with 14 GB free instead of 86 GB, and
# the job header names which (`Cloud: AWS`, #731). COMMAND's status is this
# script's. DISK_WATCH_INTERVAL is the seconds between samples.
set -uo pipefail

[ "$#" -ge 2 ] || { echo "usage: disk-watch.sh LABEL COMMAND..." >&2; exit 64; }
label="$1"
shift

free_mb() { df -Pm . 2>/dev/null | awk 'NR == 2 { print $4 }'; }

before="$(free_mb)"
lowest_file="$(mktemp "${TMPDIR:-/tmp}/disk-watch.XXXXXX")"
sampler=""
cleanup() {
    [ -z "$sampler" ] || kill "$sampler" 2>/dev/null
    rm -f -- "$lowest_file"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

printf '%s\n' "$before" >"$lowest_file"
(
    lowest="$before"
    while sleep "${DISK_WATCH_INTERVAL:-1}"; do
        now="$(free_mb)"
        case "$now" in '' | *[!0-9]*) continue ;; esac
        case "$lowest" in
            '' | *[!0-9]*) lowest="$now" ;;
            *) [ "$now" -ge "$lowest" ] || lowest="$now" ;;
        esac
        printf '%s\n' "$lowest" >"$lowest_file"
    done
) >/dev/null 2>&1 &
sampler=$!

"$@"
status=$?

kill "$sampler" 2>/dev/null
wait "$sampler" 2>/dev/null
sampler=""

after="$(free_mb)"
lowest="$(cat "$lowest_file")"
# The last sample can be a second old, and COMMAND's end may be the lowest of all.
case "$after" in '' | *[!0-9]*) ;; *) { [ -n "$lowest" ] && [ "$lowest" -le "$after" ]; } || lowest="$after" ;; esac
echo "disk: $label: ${before:-unknown} MB free before, ${lowest:-unknown} MB at the lowest, ${after:-unknown} MB after"
exit "$status"
