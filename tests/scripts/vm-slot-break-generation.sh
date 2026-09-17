#!/usr/bin/env bash
#
# vm-slot-break-generation.sh — a break deletes what it inspected.
#
# `break_lock` and `cmd_release` both used to be `rm -rf "$LOCK"` on a
# FIXED path. Between deciding a holder is stale — a decision that reads
# the record, runs a `ps`, and sometimes sleeps a second — and the `rm`
# executing, the examined lock can be gone and a different, live lock
# can occupy that path. Two waiters agreeing about one stale holder is
# enough: the first breaks and acquires, the second's `rm -rf` deletes
# the first's lock, and both boot a VM. That is the failure the slot
# exists to prevent, produced by the slot itself.
#
# THE WINDOW CANNOT BE REACHED THROUGH THE CLI. It is microseconds wide
# in a process that then exits, so these tests source the script with
# `AM_VM_SLOT_LIB=1` and call the real functions, holding one
# generation's token while the lock underneath is replaced. Testing a
# reimplementation would prove nothing about the file that ships.
#
#   bash tests/scripts/vm-slot-break-generation.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
export AM_ORACLE_VM_STATE="$sandbox/state"

# Sourcing brings `set -e` with it, which would abort this file at the
# first deliberate non-zero return — and `break_lock` returning non-zero
# is the behaviour under test. Turned back off immediately after.
export AM_VM_SLOT_LIB=1
# shellcheck source=/dev/null
source "$REPO/scripts/vm-slot.sh"
set +e

SELF="$REPO/tests/vagrant"
OTHER="$sandbox/some/other/repo/tests/vagrant"

# Lay down a lock for a given holder directory and generation token. No
# token argument means a lock with no record at all, which is the
# ordinary state of `cmd_acquire` between its `mkdir` and its `printf`.
set_lock() {
    rm -rf "$LOCK"
    mkdir -p "$LOCK"
    if [ "$#" -gt 1 ]; then
        printf '%s\t%s\t%s\t%s\n' "$1" "holder-repo" "$(date +%s)" "$2" > "$HOLDER"
    fi
}

check() {
    local want="$1" what="$2" got
    if [ -d "$LOCK" ]; then got=survived; else got=removed; fi
    if [ "$got" = "$want" ]; then
        printf 'ok    %s\n' "$what"
    else
        printf 'FAIL  %s: lock %s, expected %s\n' "$what" "$got" "$want"
        fails=$((fails + 1))
    fi
}

check_eq() {
    local got="$1" want="$2" what="$3"
    if [ "$got" = "$want" ]; then
        printf 'ok    %s\n' "$what"
    else
        printf 'FAIL  %s: got %s, expected %s\n' "$what" "$got" "$want"
        fails=$((fails + 1))
    fi
}

# --- the token cmd_acquire writes -------------------------------------

# EVERYTHING BELOW COMPARES TOKENS. NOTHING BELOW PRODUCES ONE.
#
# `set_lock` takes the token as an argument, so every test in this file
# hands itself `gen-A`, `gen-B`, `gen-live` and then checks that
# `break_lock` and `delete_generation` compare them correctly. The
# expression that actually makes a generation identifiable is never
# executed by any of them, and a comparison is only worth as much as the
# thing it compares.
#
# So a plausible simplification defeats the whole file. Replacing
#
#     "$(now)-$$-${RANDOM}"    with    "$(now)"
#
# leaves every other check here at EXIT=0 while two acquisitions in the
# same second get the SAME token — at which point a breaker that read
# the first passes its check against the second and deletes a live
# replacement's lock. That is the production failure this slot exists to
# prevent, measured elsewhere as two VMs 5401 seconds apart under a
# 5400s limit.
#
# `cmd_acquire` on a free path is `mkdir` then `printf` and returns; it
# boots nothing, so it can be called here directly. The two acquisitions
# below land in the same second and the same process, which is the case
# that matters: it is the part of the token that is neither the clock
# nor the pid that has to carry the difference.
#
# THIS CHECK USED TO BE THE DEFECT IT WAS TESTING FOR. The token was
# `"$(now)-$$-${RANDOM}"`; `$RANDOM` is 0..32767, so two draws repeat
# once in 32768 and this assertion failed that often — in a required
# gate, on a property the code did not actually have. The obvious
# reaction to a 1-in-32768 red is to loosen the assertion, which puts
# the file back where it started with a comment claiming otherwise. The
# counter is the other remedy and the right one: the tokens differ
# because nothing can make them equal, so the check asserts something
# true rather than something usually true.
rm -rf "$LOCK"
acquisitions=5
tokens=""
for _ in $(seq "$acquisitions"); do
    cmd_acquire
    tokens="$tokens$(record_field "$HOLDER" 4)"$'\n'
    cmd_release
done
gen_first="$(printf '%s' "$tokens" | head -1)"
distinct="$(printf '%s' "$tokens" | sort -u | wc -l | tr -d ' ')"

# The control. Without it, an acquire that wrote no token at all would
# compare "" against "" and report a defect nobody could read, or --
# worse, if the shape ever changes -- pass.
check_eq "$([ -n "$gen_first" ] && echo written || echo empty)" written \
    "cmd_acquire writes a generation token at all"
check_eq "$distinct" "$acquisitions" \
    "$acquisitions acquisitions in the same second and process get $acquisitions different tokens"

# WHERE THE DIFFERENCE COMES FROM, pinned separately, because the check
# above would also pass on a token that happened to straddle a second
# boundary. Five acquisitions take microseconds, so in practice the
# clock and the pid are constant across all of them and only the serial
# moves -- but "in practice" is not an assertion.
#
# `next_serial` ASSIGNS RATHER THAN PRINTS, and this is what says so. A
# `printf`-and-capture version would increment inside a subshell, leave
# the caller's `SERIAL` untouched, and hand back 1 every time.
serial_before="$SERIAL"
next_serial
serial_mid="$SERIAL"
next_serial
serial_after="$SERIAL"
check_eq "$serial_mid" "$((serial_before + 1))" \
    "next_serial advances the counter in its CALLER, not in a subshell"
check_eq "$serial_after" "$((serial_before + 2))" \
    "and again, so two names taken in one process cannot be equal"

# --- break_lock ------------------------------------------------------

# THE CONTROL. Without this a `break_lock` that refused everything would
# pass every test below, and refusing everything strands the slot for
# ever — the failure that is invisible until somebody waits an hour.
set_lock "$OTHER" "gen-A"
break_lock "the generation it was authorised against" "gen-A"
check removed "a break quoting the current generation deletes it"

# The defect itself: the lock examined is replaced before the break
# runs, and the replacement is live.
set_lock "$OTHER" "gen-A"
stale_token="gen-A"
set_lock "$OTHER" "gen-B"
break_lock "a generation that is already gone" "$stale_token"
rc=$?
check survived "a break quoting a replaced generation deletes nothing"
check_eq "$rc" 1 "and says so, rather than reporting success"
check_eq "$(record_field "$HOLDER" 4)" "gen-B" "the replacement's record is intact"

# A lock with no record at all is still breakable — that is the
# "no holder recorded" path in `cmd_acquire`, which quotes an empty
# token because there is no generation to quote.
set_lock
break_lock "no holder recorded" ""
check removed "a lock with no record is broken on an empty token"

# ...but not once a replacement HAS a record. An empty token must not
# match a real one.
set_lock "$OTHER" "gen-C"
break_lock "no holder recorded" ""
check survived "an empty token does not match a real generation"

# --- cmd_release -----------------------------------------------------

# `vm.sh down` calls release unconditionally, so this path runs far more
# often than a break does.
set_lock "$SELF" "gen-D"
cmd_release
check removed "the holder releasing its own current generation frees it"

# THE RELEASE PATH'S BINDING, exercised through the shared helper
# rather than through `cmd_release`. `cmd_release` reads the record at
# call time, so from outside the process there is no way to hand it a
# token that has since gone stale -- the window it closes is between its
# own read and its own delete. `delete_generation` takes the token, and
# it is the same code the release path runs, so this tests what ships
# rather than a story about it.
set_lock "$SELF" "gen-D"
stale_token="gen-D"
set_lock "$SELF" "gen-E"
delete_generation "$stale_token" releasing
rc=$?
check survived "a release holding a replaced generation frees nothing"
check_eq "$rc" 1 "and reports that it freed nothing"
check_eq "$(record_field "$HOLDER" 4)" "gen-E" "leaving the replacement's record intact"

# And the same helper with the live token does delete, so the check
# above is not passing because the helper refuses everything.
set_lock "$SELF" "gen-E"
delete_generation "gen-E" releasing
check removed "the helper still deletes the generation it was given"

set_lock "$OTHER" "gen-F"
cmd_release
check survived "a release from a repository that never held it frees nothing"

# CMD_RELEASE'S OWN BINDING, not just the helper's. The window it closes
# is between its own read of the record and its own delete, which is
# unreachable from outside the process -- so the READ is shadowed
# instead.
#
# `read_holder` is what is shadowed, and that matters: this test used to
# shadow `holder_field`, and when `cmd_release` was changed to take one
# snapshot it stopped calling that at all for the ownership and the
# token. The test kept passing and had stopped exercising the binding.
# Shadow the read the function actually does, or the test measures a
# path nothing takes.
set_lock "$SELF" "gen-live"
saved_read_holder="$(declare -f read_holder)"
read_holder() {
    # What the record said a moment ago: this repository's directory, and
    # a generation the live lock no longer carries.
    printf '%s\t%s\t%s\t%s\n' "$SELF" "holder-repo" "$(date +%s)" "gen-stale"
}
cmd_release
eval "$saved_read_holder"
check survived "a release deletes only the generation it read, not the path"
check_eq "$(record_field "$HOLDER" 4)" "gen-live" "and the live generation is untouched"

# --- restore_lock ----------------------------------------------------

# THE RESTORE MUST NOT NEST. `mv "$staged" "$LOCK"` onto an existing
# directory does not fail — it moves the source INSIDE the destination —
# so a restore racing an acquire would bury a dead generation inside the
# live holder's lock and report success. This is the case that
# distinguishes `mkdir` from `mv` and it is why the restore uses the
# former.
set_lock "$OTHER" "gen-G"
staged="${LOCK}.breaking.test"
rm -rf "$staged"
mkdir -p "$staged"
printf '%s\t%s\t%s\t%s\n' "$OTHER" "holder-repo" "$(date +%s)" "gen-OLD" > "$staged/holder"
restore_lock "$staged"
check survived "the live lock survives a restore that races it"
check_eq "$(record_field "$HOLDER" 4)" "gen-G" "and still holds the LIVE generation"
nested="$(find "$LOCK" -mindepth 1 -type d | wc -l | tr -d ' ')"
check_eq "$nested" 0 "with no dead generation buried inside it"
check_eq "$([ -e "$staged" ] && echo present || echo gone)" gone "and nothing is left at the staging name"
# The displaced record is KEPT rather than deleted -- it is a generation
# this process was not authorised to break, so its VM may still be up.
kept="$(find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.orphan.*' | head -1)"
check_eq "$([ -n "$kept" ] && echo kept || echo lost)" kept "the displaced record is preserved as an orphan"
check_eq "$(record_field "$kept/holder" 4)" "gen-OLD" "and it is the one that was moved aside"
rm -rf "$kept"

# And the ordinary restore, where the path is still free.
rm -rf "$LOCK"
staged="${LOCK}.breaking.test2"
rm -rf "$staged"
mkdir -p "$staged"
printf '%s\t%s\t%s\t%s\n' "$OTHER" "holder-repo" "$(date +%s)" "gen-H" > "$staged/holder"
restore_lock "$staged"
check survived "a restore onto a free path puts the lock back"
check_eq "$(record_field "$HOLDER" 4)" "gen-H" "with its record"

# --- one snapshot per decision -----------------------------------------

# THE DECISION MUST DESCRIBE A STATE THAT EXISTED. The age, the holder's
# directory, its name and its token were four separate reads of a file
# another process may replace between any two of them, so a break could
# be authorised by one generation's age, aimed at a second's process and
# carry a third's token.
record="$OTHER	holder-repo	1234567890	gen-SNAP"
check_eq "$(snapshot_field "$record" 1)" "$OTHER" "field 1 comes from the snapshot"
check_eq "$(snapshot_field "$record" 2)" "holder-repo" "field 2 comes from the same one"
check_eq "$(snapshot_field "$record" 3)" "1234567890" "and field 3"
check_eq "$(snapshot_field "$record" 4)" "gen-SNAP" "and the token, from that record"

# A record that has been REPLACED does not change a snapshot already
# taken -- which is the whole property, and the reason the accessor
# takes a string rather than re-reading the file.
set_lock "$OTHER" "gen-FIRST"
record="$(read_holder)"
set_lock "$OTHER" "gen-SECOND"
check_eq "$(snapshot_field "$record" 4)" "gen-FIRST" "a snapshot is not re-read when the file changes"
check_eq "$(holder_field 4)" "gen-SECOND" "while the live accessor sees the replacement"

# --- a break that is not authorised moves nothing ----------------------

# THE REFUSAL COMES BEFORE THE MOVE. Staging a lock aside frees its path
# for as long as it takes to put it back, and restoring is a `mkdir` a
# waiter can win -- so moving a lock this process can already see is not
# its own is a risk taken for nothing.
#
# NO RESIDUE IS NOT THE SAME AS NO MOVE. A delete that moves the lock,
# finds the token is not its own and restores it leaves exactly what a
# delete that never moved it leaves, so the residue check below held
# with the pre-move comparison deleted (rust-fs-xfs#158). Every `mv` the sourced
# functions make is recorded, which is the one thing that tells the two
# apart.
moves="$sandbox/mv.log"
mv() {
    printf '%s\n' "$*" >> "$moves"
    command mv "$@"
}
: > "$moves"
set_lock "$OTHER" "gen-LIVE"
delete_generation "gen-STALE" breaking
rc=$?
check survived "an unauthorised break leaves the lock where it is"
check_eq "$rc" 1 "and reports that it deleted nothing"
# Staging names only. An orphan would mean it moved the lock and could
# not put it back; a staging directory would mean it moved it and left
# it there. Neither may happen when the refusal comes first.
residue="$(find "$AM_ORACLE_VM_STATE" -maxdepth 1 \
    \( -name 'slot.lock.breaking.*' -o -name 'slot.lock.releasing.*' \
       -o -name 'slot.lock.orphan.*' \) | wc -l | tr -d ' ')"
check_eq "$residue" 0 "and left nothing staged"
check_eq "$(wc -l < "$moves" | tr -d ' ')" 0 "and never moved it at all, not even to put it back"
# The control: the recorder sees the move an authorised delete makes, so
# the zero above is the refusal and not a recorder that sees nothing.
: > "$moves"
set_lock "$OTHER" "gen-LIVE"
delete_generation "gen-LIVE" breaking
check removed "an authorised break deletes the lock"
check_eq "$(wc -l < "$moves" | tr -d ' ')" 1 "and the recorder saw its one move"
unset -f mv

# --- a restore that loses the race keeps the record --------------------

# THE WORST AVAILABLE ANSWER IS TO DELETE IT, which is what this did.
# The generation staged aside is one this process decided it was NOT
# authorised to break, so its VM may still be running; discarding the
# record leaves two holders and no trace of how.
rm -rf "$LOCK"
staged="${LOCK}.breaking.race"
rm -rf "$staged"
mkdir -p "$staged"
printf '%s\t%s\t%s\t%s\n' "$OTHER" "holder-repo" "$(date +%s)" "gen-DISPLACED" > "$staged/holder"
# A waiter takes the path while the lock is aside.
set_lock "$OTHER" "gen-WINNER"
restore_lock "$staged"
rc=$?
check_eq "$rc" 1 "a restore that cannot put the lock back says so"
check survived "the winner's lock is untouched"
check_eq "$(record_field "$HOLDER" 4)" "gen-WINNER" "and still holds the winner's generation"
orphan_count="$(find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.orphan.*' | wc -l | tr -d ' ')"
check_eq "$orphan_count" 1 "the displaced record is kept, not deleted"
orphan="$(find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.orphan.*' | head -1)"
check_eq "$(record_field "$orphan/holder" 4)" "gen-DISPLACED" "and it is the displaced generation"
check_eq "$([ -e "$staged" ] && echo present || echo gone)" gone "with nothing left at the staging name"
rm -rf "$orphan"

# --- a SECOND displaced record does not land on the first --------------

# `cmd_acquire` loops: a `break_lock` that returns non-zero makes it
# `continue`, so ONE process can reach the orphan path more than once.
# The name was `${LOCK}.orphan.$$` -- the same name both times -- and
# `restore_lock` `rm -rf`s that name before moving into it. So the
# second displaced record deleted the first, silently, and the first was
# a generation this process had already decided it was NOT authorised to
# break: its VM may still have been running, and the only trace of it
# was gone.
#
# Two displacements, one process, and both records must still be there.
find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.orphan.*' -exec rm -rf {} +
set_lock "$OTHER" "gen-WINNER-2"
for displaced in gen-DISPLACED-ONE gen-DISPLACED-TWO; do
    staged="${LOCK}.breaking.$displaced"
    rm -rf "$staged"
    mkdir -p "$staged"
    printf '%s\t%s\t%s\t%s\n' "$OTHER" "holder-repo" "$(date +%s)" "$displaced" \
        > "$staged/holder"
    restore_lock "$staged"
done
orphan_count="$(find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.orphan.*' | wc -l | tr -d ' ')"
check_eq "$orphan_count" 2 "two displaced records in one process are both kept"
kept_tokens="$(find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.orphan.*' \
    -exec awk -F'\t' '{print $4}' {}/holder \; | sort | tr '\n' ' ')"
check_eq "$kept_tokens" "gen-DISPLACED-ONE gen-DISPLACED-TWO " \
    "and the first is not overwritten by the second"
check survived "the winner's lock is still untouched by either"
find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.orphan.*' -exec rm -rf {} +

if [ "$fails" -eq 0 ]; then
    echo "vm-slot-break-generation: all checks passed"
else
    echo "vm-slot-break-generation: $fails check(s) failed" >&2
fi
exit $(( fails > 0 ))
