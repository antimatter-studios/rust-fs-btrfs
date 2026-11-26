#!/usr/bin/env bash
# run-bounded.sh SECONDS COMMAND...  run COMMAND, and stop it by name if it is
# still running after SECONDS.
#
# A script guard that stopped making progress held a runner until the job's
# six-hour limit, and the job said only that it was cancelled (#793, #813).
# COMMAND's status is this script's; a stopped one exits 124. COMMAND gets a
# process group of its own, so what it started is stopped with it. `timeout` is
# not on every host this runs on, so this does the job in the shell.
set -uo pipefail

[ "$#" -ge 2 ] || { echo "usage: run-bounded.sh SECONDS COMMAND..." >&2; exit 64; }
bound="$1"
shift
case "$bound" in '' | *[!0-9]*) echo "run-bounded.sh: '$bound' is not a number of seconds" >&2; exit 64 ;; esac

pid=""
stop() {
    [ -n "$pid" ] || return 0
    kill -TERM -- "-$pid" 2>/dev/null || return 0
    local tries=0
    while kill -0 -- "-$pid" 2>/dev/null && [ "$tries" -lt 20 ]; do
        sleep 0.1
        tries=$((tries + 1))
    done
    kill -KILL -- "-$pid" 2>/dev/null || true
}
trap 'stop; exit 129' HUP
trap 'stop; exit 130' INT
trap 'stop; exit 143' TERM

set -m
"$@" &
pid=$!
set +m

deadline=$((SECONDS + bound))
while kill -0 "$pid" 2>/dev/null; do
    if [ "$SECONDS" -ge "$deadline" ]; then
        echo "FAIL  still running after ${bound}s and stopped: $*" >&2
        stop
        wait "$pid" 2>/dev/null
        exit 124
    fi
    sleep 0.2
done
wait "$pid"
