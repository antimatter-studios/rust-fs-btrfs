#!/usr/bin/env bash
#
# vm-deadline-semantics.sh — the guest's own poweroff timer.
#
# The oracle VM schedules its own shutdown at boot. That exists because
# every other teardown here is passive: `defer:` needs the task to reach
# the step that registered it, `vm.sh reap` needs a later chore
# invocation, and the slot's age check runs only inside `acquire` and so
# needs another repository to want the slot. On 2026-09-07 none of those
# happened and the machine stayed up for thirteen hours holding the
# global lock, with every net behaving exactly as written.
#
# WHAT THIS TEST CAN AND CANNOT REACH. It cannot boot a VM, so it does
# not assert that a shutdown is really scheduled -- that was verified by
# hand in a live guest and the numbers are in the Vagrantfile's comment.
# What it can do is pin the two things that were WRONG when the
# mechanism was first written, both of which were silent:
#
#   1. the minutes were read from the guest's environment, which Vagrant
#      does not forward, so the knob reported the default and could not
#      be changed. Exporting 90 produced "powering off in 480".
#
#   2. `hold` invoked `vagrant ssh` without the `cd "$VAGRANT_DIR"` that
#      every other call in vm.sh uses, so it could not reach the guest
#      and the deadline it claimed to cancel still stood.
#
# Both were caught only by running the thing. A test that cannot boot a
# guest can still refuse to let them come back.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VAGRANTFILE="$REPO/tests/vagrant/debian/Vagrantfile"
VM_SH="$REPO/scripts/vm.sh"
fails=0

ok()   { printf 'ok    %s\n' "$1"; }
bad()  { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# 1. The deadline must be armed on every boot, not only on the first
#    provision. Without `run: "always"` a machine that was provisioned
#    before this existed -- or one merely halted and restarted -- comes
#    up with no timer at all, which is the exact state that leaked.
check "the deadline provisioner runs on every boot" \
  "grep -q 'provision \"deadline\", type: \"shell\", run: \"always\"' '$VAGRANTFILE'"

# 2. The minutes are interpolated by Ruby on the host. A shell expansion
#    inside the inline script reads the GUEST's environment, which is
#    empty, so the value silently falls back to the default.
check "the minutes are interpolated on the host, not expanded in the guest" \
  "grep -q 'MINS=\"#{deadline_mins}\"' '$VAGRANTFILE'"

check "no shell expansion of the deadline variable survives in the inline script" \
  "! grep -q 'MINS=\"\\\${AM_ORACLE_VM_DEADLINE_MINS' '$VAGRANTFILE'"

check "the host reads the override through ENV.fetch" \
  "grep -q 'ENV.fetch(\"AM_ORACLE_VM_DEADLINE_MINS\"' '$VAGRANTFILE'"

# 3. A previous timer must be cancelled before a new one is scheduled,
#    or a re-provision stacks two and the earlier one wins -- a machine
#    that dies sooner than the number it just printed.
# Anchored to the indented code line, not the bare string: the first
# version of this matched the word `shutdown -c` in the comment twelve
# lines above and passed with the actual call deleted. Which is the
# defect this whole repository keeps finding -- an assertion whose result
# does not depend on the thing it claims to check.
check "a previous timer is cancelled before a new one is set" \
  "grep -qE '^ +shutdown -c >/dev/null' '$VAGRANTFILE'"

# 4. The hold marker lives on tmpfs. In /etc it would survive a reboot,
#    so a machine held once would never re-arm and the exemption would
#    outlive everyone's memory of granting it.
check "the hold marker is on tmpfs so a reboot clears it" \
  "grep -q '/run/am-oracle-vm-held' '$VAGRANTFILE'"

check "the hold marker is not somewhere persistent" \
  "! grep -q '/etc/am-oracle-vm-held' '$VAGRANTFILE' '$VM_SH'"

# 5. `vm.sh hold` has to cancel the guest's timer as well as reap. A
#    hold that stopped reap but let the machine power itself off would be
#    a hold in name only.
check "hold cancels the guest deadline as well as reap" \
  "grep -q 'am-oracle-vm-held' '$VM_SH'"

# 6. And it must reach the guest the way everything else in this file
#    does. A bare `vagrant ssh` finds no machine outside VAGRANT_DIR and
#    fails silently into the error branch.
check "hold reaches the guest from VAGRANT_DIR" \
  "grep -A3 'am-oracle-vm-held' '$VM_SH' | grep -q 'cd \"\\\$VAGRANT_DIR\"'"

# 7. When it cannot reach the guest it must say the deadline still
#    stands. Reporting a hold it did not achieve is worse than failing,
#    because the machine then dies under someone who was told it would
#    not.
check "an unreachable guest is reported rather than assumed held" \
  "grep -qi 'deadline still stands' '$VM_SH'"

# ---------------------------------------------------------------------
# 8. THE SCRIPT ITSELF, RUN.
#
# Everything above is a grep, and a grep can only say a line is present.
# The defect that prompted this section was not a missing line: it was a
# present line whose failure nobody read. `nohup shutdown ... &`
# backgrounded the one call this provisioner exists to make -- which is
# precisely how a command opts out of `set -e` -- discarded its status,
# and fell through to an unconditional echo announcing a timer that may
# never have been set.
#
# So these run THE SHIPPED SCRIPT, extracted from the Vagrantfile rather
# than copied here, against a stubbed `shutdown`. A copy would drift; an
# extraction cannot.
#
# Anchored on the deadline provisioner, not on `inline: <<-SHELL` --
# there is an earlier provisioner using the same heredoc marker, and the
# first version of this extracted THAT one and would have happily tested
# the package installer.
extract_deadline_script() {
    awk '/provision "deadline"/{d=1}
         d && /inline: <<-SHELL/{f=1;next}
         f && /^[[:space:]]*SHELL[[:space:]]*$/{exit}
         f{print}' "$VAGRANTFILE"
}

# Runs the shipped script with `shutdown` stubbed and the guest's
# systemd state built as files under the sandbox.
#   $1 minutes to interpolate (Ruby does this on the host)
#   $2 exit status the `shutdown -h` stub should return, or the literal
#      NONE to leave `shutdown` off PATH entirely
#   $3 the guest's systemd state, one of:
#        armed      -- systemd running, logind has a scheduled shutdown
#        unarmed    -- systemd running, logind has scheduled nothing
#        nosystemd  -- no /run/systemd/system, so nothing can confirm
#   $4 "held" to create the hold marker, anything else for not held
# Prints the script's own output; returns the script's exit status.
run_deadline_script() {
    local mins="$1" sd_exit="$2" sysd="$3" held="$4"
    local sandbox stubs script marker rc
    sandbox="$(mktemp -d)"
    stubs="$sandbox/bin"
    mkdir -p "$stubs"
    marker="$sandbox/am-oracle-vm-held"
    [ "$held" = held ] && : > "$marker"

    script="$sandbox/deadline.sh"
    extract_deadline_script > "$script.raw"

    # A mis-extraction must not look like a passing test.
    if [ ! -s "$script.raw" ]; then
        echo "HARNESS: extracted no script from $VAGRANTFILE" >&2
        rm -rf "$sandbox"
        return 111
    fi
    if ! grep -q 'MINS=' "$script.raw"; then
        echo "HARNESS: extracted the wrong heredoc -- no MINS= in it" >&2
        rm -rf "$sandbox"
        return 111
    fi

    # Ruby interpolates the minutes on the host; the marker path is
    # redirected the same way so the hold branch is reachable without
    # writing to the real /run.
    # /run is redirected wholesale: the hold marker AND logind's
    # scheduled-shutdown record both live there, and a test may not
    # write to the real one.
    sed -e "s|#{deadline_mins}|$mins|g" \
        -e "s|/run/am-oracle-vm-held|$marker|g" \
        -e "s|/run/systemd|$sandbox/run/systemd|g" "$script.raw" > "$script"

    if [ "$sd_exit" != NONE ]; then
    cat > "$stubs/shutdown" <<STUB
#!/bin/sh
# /bin/sh by ABSOLUTE path, NOT a /usr/bin/env shebang: this stub runs
# under a PATH holding only the stub directory, so env would look up
# its interpreter there and fail. Neither stub needs bash.
#
# NO BACKTICKS ANYWHERE IN THIS HEREDOC. It is unquoted, because
# $sandbox and $sd_exit below have to expand -- which means backticks
# are command substitution too. A pair around a word in this comment
# ran that word and spliced its output into the stub: the environment
# landed in the middle of the file and the stub failed at "line 54"
# with a PATH for a command name.
# -c (cancel) always succeeds; the scheduling call is the one under test.
case "\$1" in
  -c) exit 0 ;;
esac
echo "\$@" >> "$sandbox/shutdown.args"
exit $sd_exit
STUB
    chmod +x "$stubs/shutdown"
    fi

    # THE GUEST'S SYSTEMD STATE IS A FILESYSTEM FACT, not a command's
    # output. logind writes /run/systemd/shutdown/scheduled when a
    # shutdown is scheduled and removes it on cancel, and
    # /run/systemd/system exists only where systemd is managing the
    # guest -- so "no timer" and "nothing here can tell you" are
    # distinguishable, which is exactly what the previous
    # `systemctl show -p ScheduledShutdownUSec` could not do.
    case "$sysd" in
      armed)
        mkdir -p "$sandbox/run/systemd/system" "$sandbox/run/systemd/shutdown"
        printf 'USEC=1788700000000000\nMODE=poweroff\n' \
          > "$sandbox/run/systemd/shutdown/scheduled"
        ;;
      unarmed)
        mkdir -p "$sandbox/run/systemd/system"
        ;;
      nosystemd) ;;
      *)
        echo "HARNESS: unknown systemd state $sysd" >&2
        rm -rf "$sandbox"
        return 111
        ;;
    esac

    # THE STUB DIRECTORY IS THE WHOLE PATH, and that is the point.
    #
    # This was "$stubs:/usr/bin:/bin", which let the HOST decide whether
    # a given program exists -- so a case meaning "this guest has no
    # such tool" tested the machine rather than the script. That is how
    # the systemctl arm passed on macOS and in a bare container and
    # FAILED on GitHub's ubuntu runner, where /usr/bin/systemctl is
    # real. The confirmation no longer runs a program at all, but the
    # narrowing stays: `shutdown` is still stubbed, and the "missing
    # shutdown" case must mean missing everywhere.
    #
    # The extracted script needs no other program. `command` is a shell
    # builtin and the only externals it names are `shutdown` and
    # `systemctl`, so an empty PATH beyond $stubs makes absence mean
    # absence on every host.
    # bash is resolved BEFORE the PATH is narrowed and then invoked by
    # absolute path: `PATH=x bash ...` applies the new PATH to the
    # lookup of `bash` itself, which is `command not found`.
    local shell
    shell="$(command -v bash)"
    set +e
    # stdin from /dev/null: the child runs inside `$( )`, and a
    # command substitution does not finish until every writer to the
    # pipe has exited. Leaving the child on an inherited stdin let a
    # run block indefinitely, which in CI is worse than a failure --
    # nothing is reported at all.
    PATH="$stubs" "$shell" "$script" </dev/null 2>&1
    rc=$?
    set -e
    rm -rf "$sandbox"
    return $rc
}

expect_run() {
    local label="$1" want_ok="$2" want_text="$3" absent_text="$4"
    shift 4
    local out rc
    set +e
    out="$(run_deadline_script "$@")"
    rc=$?
    set -e

    if [ "$rc" = 111 ]; then
        bad "$label (harness could not build the fixture)"
        return
    fi
    if [ "$want_ok" = ok ] && [ "$rc" != 0 ]; then
        bad "$label (expected success, got exit $rc: $out)"
        return
    fi
    if [ "$want_ok" = fail ] && [ "$rc" = 0 ]; then
        bad "$label (expected failure, got exit 0: $out)"
        return
    fi
    if [ -n "$want_text" ] && ! printf '%s' "$out" | grep -qF "$want_text"; then
        bad "$label (output lacked '$want_text': $out)"
        return
    fi
    if [ -n "$absent_text" ] && printf '%s' "$out" | grep -qF "$absent_text"; then
        bad "$label (output wrongly claimed '$absent_text': $out)"
        return
    fi
    ok "$label"
}

# THE DEFECT. A scheduling call that fails must not be reported as a
# scheduled shutdown. This is the assertion the backgrounded version
# could not satisfy: it exited 0 and printed the success line.
expect_run "a shutdown that cannot be scheduled fails loudly" \
    fail "FAILED to schedule" "powering off in 480 minutes" \
    480 1 armed notheld

# And `shutdown` missing altogether is the same class of failure.
expect_run "a missing shutdown command fails rather than reporting success" \
    fail "" "powering off in 480 minutes" \
    480 NONE armed notheld

# The success path: logind has a record, so the timer is confirmed.
expect_run "a scheduled shutdown reports the armed timer" \
    ok "powering off in 480 minutes" "" \
    480 0 armed notheld

# THE THREE STATES THAT USED TO BE ONE. systemd is running, so
# logind's record is authoritative and its absence is a real answer:
# shutdown returned 0 and scheduled nothing.
expect_run "an accepted request that logind did not record is a failure" \
    fail "logind has scheduled nothing" "powering off in 480 minutes" \
    480 0 unarmed notheld

# Where nothing can confirm, say so rather than claiming the timer is
# missing. The previous version aborted provisioning here, on a guest
# that may well have had a perfectly good timer.
expect_run "a guest with no systemd is reported as unconfirmed, not unarmed" \
    ok "no systemd to confirm" "logind has scheduled nothing" \
    480 0 nosystemd notheld

# A held machine schedules nothing and still succeeds.
expect_run "a held machine schedules no shutdown" \
    ok "no shutdown scheduled" "powering off in 480 minutes" \
    480 0 armed held

# The specific construct that caused this, kept out by name.
# Anchored to an indented CODE line. The unanchored version matched the
# comment above the fix that names the old construct, and so failed
# against a Vagrantfile that no longer contains it -- the same trap the
# `shutdown -c` assertion above documents, met again in the same file.
check "the scheduling call is not backgrounded out of set -e's reach" \
  "! grep -qE '^ +nohup +shutdown' '$VAGRANTFILE'"

# ---------------------------------------------------------------------
# 9. THE MINUTES ARE VALIDATED BEFORE THEY BECOME SHELL TEXT.
#
# deadline_mins is spliced verbatim into a heredoc that Vagrant runs as
# root in the guest. The default is a safe literal, but nothing checked
# a caller-supplied override.
#
# The pattern is READ OUT OF THE VAGRANTFILE rather than restated here,
# so this cannot pass against a pattern the Vagrantfile does not use.
deadline_re="$(sed -n 's|.*deadline_mins =~ \(/.*/\).*|\1|p' "$VAGRANTFILE")"
if [ -z "$deadline_re" ]; then
    bad "the minutes are validated before interpolation (no pattern found)"
elif ! command -v ruby >/dev/null 2>&1; then
    bad "the minutes are validated before interpolation (no ruby to evaluate it)"
else
    # `--` matters: without it `ruby -e PROG -1` treats -1 as a ruby
    # option, ruby exits non-zero, and the harness scores that as the
    # pattern having refused the input. A check whose result does not
    # depend on the thing it checks.
    accepts() { ruby -e 'exit(ARGV[0] =~ '"$deadline_re"' ? 0 : 1)' -- "$1"; }

    if accepts 480; then ok "the default 480 is accepted"
    else bad "the default 480 is accepted"; fi

    # Each of these reaches a privileged guest shell if it gets through.
    for evil in "" "0" "abc" "480; rm -rf /" "480 && reboot" "-1" "\$(id)" "480
rm -rf /"; do
        if accepts "$evil"; then
            bad "a non-integer override is refused (accepted $(printf '%q' "$evil"))"
        else
            ok "a non-integer override is refused ($(printf '%q' "$evil"))"
        fi
    done

    # ^ and $ are LINE anchors in Ruby, so /^[0-9]+$/ would accept the
    # embedded-newline case above and splice a second command into the
    # script. That case is in the loop; this pins the cause.
    case "$deadline_re" in
        *'\A'*'\z'*) ok "the pattern is anchored on the whole string, not per line" ;;
        *) bad "the pattern uses line anchors, so a newline smuggles a second command" ;;
    esac
fi

echo
if [ "$fails" = 0 ]; then
    echo "PASS  vm deadline semantics"
else
    echo "FAIL  $fails assertion(s)"
    exit 1
fi
