#!/usr/bin/env bash
#
# vm-slot.sh — one oracle VM at a time, across every repository.
#
#   vm-slot.sh acquire     wait for the slot, then take it
#   vm-slot.sh release     give it back (idempotent)
#   vm-slot.sh status      say who holds it and for how long
#
# WHY THIS EXISTS. These VMs are not small: this one asks for 4 GB and
# the NTFS Windows box asks for 8. Two of them plus a `cargo test` fills
# a laptop, and when it fills, the machine does not fail cleanly -- it
# starts killing background work. That is exactly what happened on the
# night several agents were building fixtures in parallel: two VMs left
# running for hours, and unrelated jobs killed for want of memory with
# no message that connected the two.
#
# So the slot is deliberately a single global one rather than one per
# repository. The cost is real and worth stating: fixture builds in
# different repositories no longer overlap, and a queued build waits for
# the one ahead of it. That is slower on a good day and much better on a
# bad one, because the failure it removes was silent and the cost it
# adds is visible.
#
# The state lives outside every repository, because the whole point is
# that repositories do not know about each other.
set -euo pipefail

STATE_DIR="${AM_ORACLE_VM_STATE:-$HOME/.local/state/am-oracle-vm}"
LOCK="$STATE_DIR/slot.lock"
HOLDER="$LOCK/holder"

# How long to wait before giving up, and how long a holder may keep the
# slot before another waiter is allowed to take it away.
#
# The wait is long because a fixture build legitimately takes tens of
# minutes; the breaking point is longer still, because breaking a lock
# someone is genuinely using is worse than waiting. Both are overridable
# for a caller that knows better.
WAIT_SECS="${AM_ORACLE_VM_WAIT:-3600}"
STALE_SECS="${AM_ORACLE_VM_STALE:-5400}"

# How long a freshly taken slot is trusted before the "is a VM actually
# running" test is allowed to break it.
#
# THE SLOT IS TAKEN BEFORE THE VM EXISTS -- necessarily, since the point
# is to stop a second one booting. So for the length of a `vagrant up`
# there is a holder with no VM behind it, and without this a waiter
# would look, see nothing running, break the lock, and boot the second
# VM this whole file exists to prevent. Both would then be up and each
# would think it held the slot.
#
# Three minutes covers a cold boot with provisioning on this hardware.
# Too long only delays reclaiming a genuinely dead lock; too short
# reintroduces the race, so it errs long.
BOOT_GRACE_SECS="${AM_ORACLE_VM_BOOT_GRACE:-180}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_NAME="$(basename "$REPO")"
# The directory whose VM this slot is being held for. It is what makes
# staleness answerable: a running qemu names its disk image, and that
# path is under here.
VAGRANT_DIR="$REPO/tests/vagrant"

now() { date +%s; }

# EVERY NAME THIS FILE INVENTS CARRIES ONE OF THESE, AND IT IS A COUNTER
# RATHER THAN A DRAW.
#
# Two things here have to be unique per process and per second: the
# generation token, so a breaker holding one generation's token cannot
# match the generation that replaced it; and the directory a displaced
# record is orphaned into, so a second displaced record does not land on
# the first.
#
# Both used `$RANDOM` or nothing at all. `$RANDOM` is 0..32767, so two
# draws repeat once in 32768 -- and the test that asserts two
# acquisitions differ then fails that often and takes a required gate
# with it. "Flaky, retry it" is the wrong reading, because when it fires
# it is telling the truth: two generations really were given one
# identity. A counter cannot repeat, so the property the test asserts is
# one the code actually has.
#
# NOT A COMMAND SUBSTITUTION, and this is the whole reason it assigns to
# a variable instead of printing. `x="$(next_serial)"` would run the
# increment in a SUBSHELL, leaving `SERIAL` at its old value in the
# caller -- so every call would yield 1, which reads as working because
# 1 differs from whatever came before it often enough to look fine.
SERIAL=0
next_serial() { SERIAL=$((SERIAL + 1)); }

# The holder record, or empty if the slot is free.
#   vagrant_dir<TAB>repo<TAB>epoch
#
# NOT A PID. `acquire` is its own short-lived process -- it takes the
# slot and exits, leaving the VM behind -- so a PID recorded here is
# dead within milliseconds and every lock would look stale immediately.
# The first version of this file did exactly that, and its own status
# command reported "that process is gone" one line after acquiring.
#
# What actually holds the slot is a RUNNING VM, so that is what is
# recorded and what staleness is measured against.
read_holder() {
    [ -f "$HOLDER" ] || return 1
    cat "$HOLDER" 2>/dev/null
}

# `awk` rather than `cut`, because `cut`'s answer for a record with no
# tab is the WHOLE LINE. A malformed record would then hand back its own
# text as every field, including the generation token below, and a token
# that is accidentally equal is the one failure this whole mechanism
# exists to prevent.
holder_field() { read_holder | awk -F'\t' -v n="$1" '{print $n}'; }

# A field of a record ALREADY READ.
#
# ONE READ PER DECISION. `holder_field` re-reads the file every call, so
# asking it for the age, the holder and the token gave three answers
# about three possibly-different records -- and a decision assembled
# from three snapshots can describe a state that never existed. That is
# the same two-reads-of-a-changing-record shape as the defect this file
# exists to fix, reproduced in the code doing the fixing.
snapshot_field() { printf '%s\n' "$1" | awk -F'\t' -v n="$2" '{print $n}'; }

# The same accessor for a record that is not at `$HOLDER` -- a lock that
# has been moved aside to be examined.
# The `-f` guard is not decoration. `< "$1"` on a missing file is a
# REDIRECTION failure, which bash reports itself -- `2>/dev/null` on the
# command silences awk and not the shell -- so a lock with no record
# printed a "No such file or directory" line from inside a function
# whose whole job is to answer quietly.
record_field() {
    [ -f "$1" ] || return 0
    awk -F'\t' -v n="$2" '{print $n}' < "$1" 2>/dev/null
}

pretty_age() {
    local secs=$1
    if [ "$secs" -lt 60 ]; then echo "${secs}s"
    elif [ "$secs" -lt 3600 ]; then echo "$((secs / 60))m"
    else echo "$((secs / 3600))h$(((secs % 3600) / 60))m"
    fi
}

# A lock is stale when no VM is actually running for the directory that
# took it. A qemu process names its disk image on the command line, and
# that image lives under the holder's vagrant directory, so one `ps` is
# enough and no cooperation from the holder is needed.
#
# This is the check that survives a crash: a script killed between
# booting a VM and halting it leaves both the lock and the VM, and the
# VM is the thing worth noticing. A script killed before the VM came up
# leaves a lock nothing is using, and this frees it.
holder_is_dead() {
    local dir procs
    # From the caller's snapshot, so the process this looks for belongs
    # to the same record the age and the token came from.
    dir="$1"
    [ -n "$dir" ] || return 0

    # NO `grep -q` ON THE END OF THIS PIPELINE, and the reason is worth
    # keeping. `set -o pipefail` is on at the top of this file. `grep -q`
    # exits the instant it matches, which closes the pipe, which sends
    # `ps` a SIGPIPE, which makes the PIPELINE exit 141 -- *because* the
    # match succeeded. Negated, that reads as "no VM is running", so the
    # check reported the opposite of the truth precisely when it found
    # what it was looking for, and every held slot looked stale.
    #
    # Caught by running it against a VM that was demonstrably up: `ps`
    # by hand found the process, and this function said it had not.
    #
    # A grep without `-q` reads its input to the end, so nothing gets a
    # SIGPIPE, and the match is then tested against a plain string.
    procs="$(ps -eo args 2>/dev/null | grep qemu || true)"
    case "$procs" in
        *"$dir"*) return 1 ;;   # a VM is running for it: not dead
    esac
    return 0
}

# Return a lock that was moved aside to the live path, or drop it if
# somebody has taken the path in the meantime.
#
# `mkdir` RATHER THAN `mv "$staged" "$LOCK"`, and the difference is not
# cosmetic: `mv` of a directory onto an existing directory does not fail,
# it moves the source INSIDE the destination. So a restore racing an
# acquire would bury a dead generation inside the live holder's lock and
# report success. `mkdir` is the same atomic primitive `cmd_acquire`
# uses, and it fails cleanly when the path is taken -- at which point the
# staged copy is a record of a generation that is over and is dropped.
restore_lock() {
    local staged="$1" orphan
    if mkdir "$LOCK" 2>/dev/null; then
        mv "$staged/holder" "$HOLDER" 2>/dev/null || true
        rm -rf "$staged"
        return 0
    fi
    # THE PATH IS TAKEN AND THIS RECORD CANNOT GO BACK. It used to be
    # deleted here, which is the worst available answer: the generation
    # staged aside was one this process had already decided it was NOT
    # authorised to break, so its VM may well still be running, and
    # discarding its record leaves two holders and no trace of how.
    #
    # Kept instead, under a name nothing looks for, and reported. A
    # visible orphan beside the lock is a state somebody can diagnose;
    # a silent double-hold is the failure this file exists to prevent.
    #
    # THE SERIAL IS WHAT MAKES THAT TRUE MORE THAN ONCE. The name was
    # `${LOCK}.orphan.$$`, and one process reaches here twice: a failed
    # `break_lock` makes `cmd_acquire` `continue`, so the loop can
    # displace a second record while the first is still parked. With the
    # pid alone both wanted the same name and the `rm -rf` below threw
    # the first record away -- losing exactly the evidence this branch
    # exists to keep, and doing it silently.
    next_serial
    orphan="${LOCK}.orphan.$$.$SERIAL"
    # Nothing can be at a name no other call can produce. Kept anyway
    # because `mv` onto an EXISTING directory does not fail, it moves
    # the source inside -- the trap `restore_lock` was written around,
    # and one an empty directory left by anything else would spring.
    rm -rf "$orphan"
    if mv "$staged" "$orphan" 2>/dev/null; then
        echo "[vm-slot] a lock was staged aside and the slot was retaken before it" >&2
        echo "[vm-slot] could be restored; the displaced record is at $orphan" >&2
        echo "[vm-slot] and its VM may still be running -- check before trusting" >&2
        echo "[vm-slot] the slot." >&2
    fi
    return 1
}

# Delete the generation this break was authorised against, or nothing.
#
# THIS WAS `rm -rf "$LOCK"`, AND `$LOCK` IS A FIXED PATH. Between a
# waiter deciding a holder is stale -- a decision that reads the record,
# runs a `ps`, and sometimes sleeps a second -- and the `rm` executing,
# the lock it examined can be gone and a different, live lock can occupy
# that path. Two waiters agreeing about one stale holder is enough: the
# first breaks and acquires, the second's `rm -rf` deletes the FIRST's
# lock, and both run a VM. That is the failure this file exists to
# prevent, produced by the file itself.
#
# THE MOVE COMES FIRST AND THE QUESTION AFTER. `mv` within one directory
# is atomic, so from the moment it returns the live path is free and
# nobody can be acquiring the thing being examined. Then the moved
# copy's token is compared against the one the caller was authorised
# with, and a mismatch puts it back rather than deleting it. Checking
# before the move leaves exactly the window the token exists to close.
# Delete the lock if it still holds the generation named, and otherwise
# leave it exactly as found. 0 when it was deleted, 1 when it was not.
#
# ONE IMPLEMENTATION FOR BOTH CALLERS, because a break and a release are
# the same operation seen from two sides, and the release path is the
# one that runs constantly -- `vm.sh down` calls it whether or not this
# repository ever booted anything. Having it take the token as an
# argument is also what makes it testable: the window is microseconds
# wide in a process that then exits, so a test cannot reach it through
# the CLI, but it can hold one generation's token and present it after
# the lock underneath has been replaced.
delete_generation() {
    local token="${1-}" tag="$2" staged
    # REFUSED BEFORE THE MOVE AS WELL AS AFTER, and both checks earn
    # their place.
    #
    # The one AFTER is the load-bearing one: `mv` is atomic, so from the
    # moment it returns nobody can be acquiring the thing being
    # examined, and comparing there is what stops a break deleting a
    # generation that replaced the one it inspected.
    #
    # The one BEFORE exists because the after-check's remedy is not
    # free. Restoring a lock is `mkdir` on a path that is briefly empty,
    # and a waiter can win it -- so moving a lock this process can
    # ALREADY SEE is not its own is a risk taken for nothing. This makes
    # that the rare case rather than the ordinary one: the only way to
    # reach the after-check now is a record that changed between these
    # two lines.
    if [ "$(holder_field 4 2>/dev/null || true)" != "$token" ]; then
        return 1
    fi
    # Serialised for the same reason the orphan is: `break_lock` can be
    # called repeatedly from one `cmd_acquire` loop, and a name that
    # repeats is a name that can collide with a copy still parked under
    # it.
    next_serial
    staged="${LOCK}.${tag}.$$.$SERIAL"
    rm -rf "$staged"
    mv "$LOCK" "$staged" 2>/dev/null || return 1
    if [ "$(record_field "$staged/holder" 4)" != "$token" ]; then
        restore_lock "$staged"
        return 1
    fi
    rm -rf "$staged"
}

break_lock() {
    local why="$1" token="${2-}"
    delete_generation "$token" breaking || return 1
    # Announced AFTER the fact, so the line describes something that
    # happened rather than something that was attempted.
    echo "[vm-slot] breaking the lock: $why" >&2
}

cmd_acquire() {
    mkdir -p "$STATE_DIR"
    local waited=0 announced=0

    while :; do
        # `mkdir` is the atomic step. Two processes racing here, one wins
        # and the other loops -- which is the whole reason the lock is a
        # directory rather than a file somebody has to check-then-write.
        if mkdir "$LOCK" 2>/dev/null; then
            # THE FOURTH FIELD IS THIS GENERATION'S IDENTITY. `mkdir`
            # makes the DIRECTORY unique; this makes the RECORD unique,
            # which is what a breaker compares against before deleting
            # anything. Without it every generation at this path looks
            # like every other one.
            next_serial
            printf '%s\t%s\t%s\t%s\n' "$VAGRANT_DIR" "$REPO_NAME" "$(now)" \
                "$(now)-$$-$SERIAL" > "$HOLDER"
            [ "$announced" = 1 ] && echo "[vm-slot] got the slot after $(pretty_age $waited)" >&2
            return 0
        fi

        # Somebody holds it. Decide whether they still exist.
        if ! read_holder >/dev/null 2>&1; then
            # The directory exists with no holder file: a process died
            # between the two steps. Give it a moment in case it is
            # simply mid-write, then take it.
            sleep 1
            # No token to quote: a lock with no record has no
            # generation, and `record_field` on a missing file is empty
            # too, so the comparison in `break_lock` still means
            # something -- a REPLACEMENT that arrived with a record will
            # not match, and is left alone.
            read_holder >/dev/null 2>&1 || { break_lock "no holder recorded" ""; continue; }
        fi

        # ONE SNAPSHOT, AND EVERY PART OF THE DECISION COMES FROM IT.
        # The age, the holder's directory, its name and its generation
        # token were four separate reads of a file another process is
        # free to replace between any two of them -- so the break could
        # be authorised by one generation's age, aimed at a second one's
        # process, and carry a third one's token. Reading once means the
        # decision describes a state that actually existed.
        local snapshot dir repo since age token
        snapshot="$(read_holder 2>/dev/null || true)"
        dir="$(snapshot_field "$snapshot" 1)"
        repo="$(snapshot_field "$snapshot" 2)"
        since="$(snapshot_field "$snapshot" 3)"
        token="$(snapshot_field "$snapshot" 4)"
        age=$(( $(now) - ${since:-0} ))

        if [ "$age" -gt "$BOOT_GRACE_SECS" ] && holder_is_dead "$dir"; then
            break_lock "no VM is running for $repo after $(pretty_age $age)" "$token"
            continue
        fi
        if [ "$age" -gt "$STALE_SECS" ]; then
            break_lock \
                "held by $repo for $(pretty_age $age), past the $(pretty_age $STALE_SECS) limit" \
                "$token"
            continue
        fi

        if [ "$announced" = 0 ]; then
            echo "[vm-slot] waiting for the oracle slot — held by $repo for $(pretty_age $age)" >&2
            echo "[vm-slot] one VM runs at a time; this is a queue, not a failure" >&2
            announced=1
        fi

        if [ "$waited" -ge "$WAIT_SECS" ]; then
            echo "[vm-slot] gave up after $(pretty_age $waited) waiting for $repo" >&2
            echo "[vm-slot] if that repository is finished, run its 'scripts/vm.sh down'," >&2
            echo "[vm-slot] or 'scripts/vm-slot.sh release --force' to take the slot." >&2
            return 1
        fi

        sleep 5
        waited=$((waited + 5))
    done
}

cmd_release() {
    # Only the holder may release, unless forced. Otherwise a script
    # that never took the slot can free somebody else's, which is the
    # same bug as not having a lock at all.
    if [ "${1:-}" = "--force" ]; then
        rm -rf "$LOCK"
        return 0
    fi
    # ONE SNAPSHOT, for the same reason as the break path. The
    # ownership test and the token were two reads: a release could
    # confirm the slot was ITS OWN from one record and then delete
    # whatever the token from a later record happened to match.
    local snapshot
    snapshot="$(read_holder 2>/dev/null || true)"
    if [ -z "$snapshot" ]; then
        # The lock exists with no holder recorded: somebody is between
        # `mkdir` and the write. Not ours to free -- acquire reclaims it.
        return 0
    fi
    if [ "$(snapshot_field "$snapshot" 1)" != "$VAGRANT_DIR" ]; then
        # Somebody else's slot. Leave it alone, and say nothing:
        # `down` calls this unconditionally, and a repository halting
        # a VM it never booted is ordinary rather than an error.
        return 0
    fi
    # THE SAME BINDING AS `break_lock`, FOR THE SAME REASON. This
    # validated `holder_field 1` and then, as a separate operation, ran
    # `rm -rf "$LOCK"`. A release that checks the old holder and deletes
    # its replacement is the identical defect on the release path -- and
    # `vm.sh down` calls this unconditionally, so it runs far more often
    # than a break does.
    delete_generation "$(snapshot_field "$snapshot" 4)" releasing
    # A release says nothing either way: `down` calls it unconditionally
    # and a repository that held nothing is not an error.
    return 0
}

cmd_status() {
    # One snapshot here too. Nothing is deleted on this path, so a mixed
    # read would only misdescribe rather than destroy -- but a status
    # line reporting one generation's age beside another's name is a
    # report of a state that never existed, which is the thing a person
    # runs `status` to avoid.
    local snapshot age
    snapshot="$(read_holder 2>/dev/null || true)"
    if [ -z "$snapshot" ]; then
        echo "the oracle slot is free"
        return 0
    fi
    age=$(( $(now) - $(snapshot_field "$snapshot" 3) ))
    printf 'held by %s for %s\n' "$(snapshot_field "$snapshot" 2)" "$(pretty_age $age)"
    if holder_is_dead "$(snapshot_field "$snapshot" 1)"; then
        if [ "$age" -le "$BOOT_GRACE_SECS" ]; then
            echo "  ...no VM yet, but it is within the $(pretty_age $BOOT_GRACE_SECS) boot window"
        else
            echo "  ...but no VM is running for it; the next acquire will take the slot"
        fi
    fi
    return 0
}

# SOURCEABLE, SO THE FUNCTIONS CAN BE TESTED DIRECTLY.
#
# `break_lock`'s whole job is to refuse a deletion authorised against a
# generation that has since been replaced, and driven through the CLI
# that window is a few microseconds wide -- it cannot be reached from
# outside the process. With `AM_VM_SLOT_LIB` set this file defines its
# functions and stops, so a test can hold one generation's token, let
# the lock be replaced, and present the stale token to the REAL function
# rather than to a reimplementation of it.
#
# Without the guard the `case` below runs at source time and exits 2,
# and a test that sourced the file would fail with "command not found"
# rather than with anything about locks.
[ -n "${AM_VM_SLOT_LIB:-}" ] && return 0

case "${1:-}" in
    acquire) cmd_acquire ;;
    release) shift; cmd_release "${1:-}" ;;
    status)  cmd_status ;;
    *)
        echo "usage: vm-slot.sh {acquire|release [--force]|status}" >&2
        exit 2
        ;;
esac
