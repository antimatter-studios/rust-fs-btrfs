#!/usr/bin/env bash
# soak-timing-guards.sh [DISK_RUNS [REGISTRY_RUNS]]  run the two guards whose
# verdict has depended on timing, over and over (#813).
#
#   * tests/scripts/disk-watch.sh, DISK_RUNS times (200), each bounded at 30 s:
#     it once stopped making progress until the job's time limit (#793).
#   * tests/scripts/fixture-pool-registry.sh, REGISTRY_RUNS times (20), beside
#     busy processes, 4 per CPU and 64 at most, so a runner that shares its
#     machine is not buried: a loaded host once failed its holder check
#     (#784). The load average is printed before each run.
#
# CI runs it on a pull request whose title holds [soak]. Every run's output goes
# to tmp/logs/soak.log; a failing run is named and the status is 1.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
disk_runs="${1:-200}"
registry_runs="${2:-20}"
log="$REPO/tmp/logs/soak.log"
mkdir -p "$(dirname "$log")"
: >"$log"

# repeat NAME TIMES BOUND SCRIPT: run SCRIPT TIMES times, each stopped at BOUND
# seconds, and say how many passed.
fails=0
repeat() {
    local name="$1" times="$2" bound="$3" script="$4" run passed=0 started
    for run in $(seq 1 "$times"); do
        started=$SECONDS
        echo "== $name run $run, load $(uptime | sed 's/.*load average[s]*: //')" | tee -a "$log"
        if bash "$REPO/scripts/run-bounded.sh" "$bound" bash "$REPO/$script" >>"$log" 2>&1; then
            passed=$((passed + 1))
            echo "   passed in $((SECONDS - started))s"
        else
            echo "FAIL  $name run $run, status $? after $((SECONDS - started))s" | tee -a "$log"
            tail -n 15 "$log"
            fails=$((fails + 1))
        fi
    done
    echo "$name: $passed of $times runs passed, each bounded at ${bound}s" | tee -a "$log"
}

repeat disk-watch "$disk_runs" 30 tests/scripts/disk-watch.sh

hogs=()
stop_load() {
    [ "${#hogs[@]}" -eq 0 ] || kill "${hogs[@]}" 2>/dev/null
    hogs=()
}
trap stop_load EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
cpus="$(getconf _NPROCESSORS_ONLN)"
busy=$((cpus * 4))
[ "$busy" -le 64 ] || busy=64
for _ in $(seq 1 "$busy"); do
    yes >/dev/null &
    hogs+=("$!")
done
echo "load: ${#hogs[@]} busy processes on $cpus CPUs; letting the load average rise" | tee -a "$log"
sleep 60

repeat fixture-pool-registry "$registry_runs" 600 tests/scripts/fixture-pool-registry.sh
stop_load

[ "$fails" -eq 0 ]
