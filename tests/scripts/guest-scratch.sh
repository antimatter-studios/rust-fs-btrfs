#!/usr/bin/env bash
# Exercise guest scratch ownership and failure propagation without a VM.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok() { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$((fails + 1)); }
mkdir -p "$REPO/tmp"
box="$(mktemp -d "$REPO/tmp/guest-scratch.XXXXXX")"
trap 'rm -rf "$box"' EXIT
mkdir -p "$box/bin" "$box/repository with spaces"
export SCRATCH_TEST_BOX="$box" SCRATCH_REAL_MKTEMP
SCRATCH_REAL_MKTEMP="$(command -v mktemp)"
export SCRATCH_REAL_CP
SCRATCH_REAL_CP="$(command -v cp)"
cat > "$box/bin/mktemp" <<'STUB'
#!/usr/bin/env bash
exec "$SCRATCH_REAL_MKTEMP" -d "$SCRATCH_TEST_BOX/storage.XXXXXX"
STUB
cat > "$box/bin/mountpoint" <<'STUB'
#!/usr/bin/env bash
exit "${SCRATCH_ALREADY_MOUNTED:-1}"
STUB
cat > "$box/bin/sync" <<'STUB'
#!/usr/bin/env bash
[[ "${SCRATCH_SYNC_FAIL:-0}" == 0 ]] || exit 74
touch "$SCRATCH_TEST_BOX/synced"
STUB
cat > "$box/bin/cp" <<'STUB'
#!/usr/bin/env bash
[[ "${SCRATCH_COPY_FAIL:-0}" == 0 ]] || exit 74
exec "$SCRATCH_REAL_CP" "$@"
STUB
cat > "$box/bin/mount" <<'STUB'
#!/usr/bin/env bash
[[ "${SCRATCH_BIND_FAIL:-0}" == 0 ]] || exit 32
[[ "$1" == --bind && -d "$2" && "$3" == "$SCRATCH_TEST_BOX/repository with spaces/tmp" ]] || exit 33
printf '%s\n' "$2" > "$SCRATCH_TEST_BOX/mounted"
STUB
cat > "$box/bin/umount" <<'STUB'
#!/usr/bin/env bash
[[ "${SCRATCH_UNMOUNT_FAIL:-0}" == 0 ]] || exit 32
[[ "$1" == "$SCRATCH_TEST_BOX/repository with spaces/tmp" ]] || exit 33
mv "$SCRATCH_TEST_BOX/mounted" "$SCRATCH_TEST_BOX/unmounted"
STUB
cat > "$box/bin/suite" <<'STUB'
#!/usr/bin/env bash
[[ -f "$SCRATCH_TEST_BOX/mounted" && "$1" == 'argument with spaces' ]] || exit 34
touch "$SCRATCH_TEST_BOX/ran"
if [[ "${SCRATCH_SUITE_FILES:-0}" == 1 ]]; then
    storage="$(cat "$SCRATCH_TEST_BOX/mounted")"
    mkdir -p "$storage/nested"
    printf 'suite artifact\n' > "$storage/nested/artifact"
fi
if [[ "${SCRATCH_SUITE_LOGS:-0}" == 1 ]]; then
    storage="$(cat "$SCRATCH_TEST_BOX/mounted")"
    mkdir -p "$storage/logs"
    printf 'oracle evidence\n' > "$storage/logs/oracle.json"
fi
exit "${SCRATCH_SUITE_STATUS:-0}"
STUB
chmod +x "$box/bin/"*
run() {
    PATH="$box/bin:$PATH" FLTH_GUEST=1 bash "$REPO/scripts/guest-scratch.sh" \
        "$box/repository with spaces" suite 'argument with spaces' > "$box/output" 2>&1
}
reset_case() {
    rm -f "$box/ran" "$box/mounted" "$box/unmounted" "$box/synced"
    unset SCRATCH_BIND_FAIL SCRATCH_UNMOUNT_FAIL SCRATCH_SUITE_STATUS SCRATCH_ALREADY_MOUNTED SCRATCH_SYNC_FAIL SCRATCH_SUITE_FILES SCRATCH_SUITE_LOGS SCRATCH_COPY_FAIL
}
reset_case
if run && [[ -f "$box/ran" && -f "$box/synced" && -f "$box/unmounted" && ! -d "$(cat "$box/unmounted")" ]]; then
    ok "the suite runs while mounted, retains arguments, syncs, and releases its scratch directory"
else fail "successful suite setup or teardown failed: $(cat "$box/output")"; fi
reset_case
export SCRATCH_SUITE_STATUS=19
run; rc=$?
if [[ "$rc" == 19 && -f "$box/synced" && -f "$box/unmounted" && ! -d "$(cat "$box/unmounted")" ]]; then
    ok "a suite failure retains its status and still releases scratch"
else fail "suite failure status or cleanup was lost ($rc)"; fi
reset_case
export SCRATCH_SYNC_FAIL=1
run; rc=$?
if [[ "$rc" != 0 && -f "$box/unmounted" && ! -d "$(cat "$box/unmounted")" ]] && grep -q 'could not be synced' "$box/output"; then
    ok "a failed sync fails the run and still releases scratch"
else fail "a failed sync was hidden or prevented cleanup ($rc)"; fi
reset_case
export SCRATCH_BIND_FAIL=1
run; rc=$?
if [[ "$rc" == 32 && ! -e "$box/ran" && ! -e "$box/unmounted" && -z "$(find "$box" -maxdepth 1 -name 'storage.*' -print)" ]]; then
    ok "a failed bind does not run the suite or leave allocated scratch"
else fail "a failed bind was accepted or leaked scratch ($rc)"; fi
reset_case
export SCRATCH_UNMOUNT_FAIL=1
run; rc=$?
if [[ "$rc" != 0 && -f "$box/ran" && -f "$box/mounted" ]] && grep -q 'TEARDOWN FAILED' "$box/output"; then
    ok "a failed unmount fails the run and names the retained mount"
else fail "a failed unmount was hidden ($rc)"; fi
reset_case
export SCRATCH_ALREADY_MOUNTED=0
run; rc=$?
if [[ "$rc" != 0 && ! -e "$box/ran" ]]; then
    ok "an existing mount cannot be covered by another run"
else fail "an existing mount was covered"; fi
reset_case
export SCRATCH_SUITE_FILES=1
run; rc=$?
if [[ "$rc" == 0 && -f "$box/unmounted" && ! -d "$(cat "$box/unmounted")" ]]; then
    ok "owned suite artifacts are removed after the mount is released"
else fail "owned suite artifacts prevented scratch cleanup ($rc)"; fi
reset_case
export SCRATCH_SUITE_LOGS=1
mkdir -p "$box/repository with spaces/tmp/logs"
printf 'tier evidence\n' > "$box/repository with spaces/tmp/logs/tier.log"
run; rc=$?
if [[ "$rc" == 0 && "$(cat "$box/repository with spaces/tmp/logs/oracle.json")" == 'oracle evidence' && "$(cat "$box/repository with spaces/tmp/logs/tier.log")" == 'tier evidence' && ! -d "$(cat "$box/unmounted")" ]]; then
    ok "oracle evidence reaches host logs without replacing tier evidence"
else fail "oracle evidence was lost or scratch was retained ($rc)"; fi
reset_case
export SCRATCH_SUITE_LOGS=1 SCRATCH_COPY_FAIL=1
run; rc=$?
if [[ "$rc" != 0 && -f "$box/unmounted" && -f "$(cat "$box/unmounted")/logs/oracle.json" ]] && grep -q 'oracle logs remain' "$box/output"; then
    ok "a failed evidence copy fails the run and retains its source logs"
else fail "a failed evidence copy was hidden or lost logs ($rc)"; fi
if FLTH_GUEST=0 bash "$REPO/scripts/guest-scratch.sh" "$box" true > "$box/output" 2>&1; then
    fail "host invocation was accepted"
else ok "host invocation is refused"; fi
[[ "$fails" == 0 ]] || exit 1
echo "guest-scratch: all checks passed"
