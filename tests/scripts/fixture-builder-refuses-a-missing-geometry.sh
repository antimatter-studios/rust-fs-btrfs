#!/usr/bin/env bash
#
# fixture-builder-refuses-a-missing-geometry.sh — a geometry that fails to
# build fails the fixture step (#140).
#
# `build-fixtures-native.sh` aborted only when the default image was
# missing, so a `dup` image mkfs.btrfs rejected left the step green and
# the dup suites skipped. This runs the real builder in a sandbox copy of
# the repository, with a two-geometry list and `mkfs.btrfs`, `btrfs` and
# `sudo` stubbed, and asserts on the script's exit status: which is what
# the CI step reads, rather than anything a test prints.
#
#   bash tests/scripts/fixture-builder-refuses-a-missing-geometry.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

mkdir -p "$sandbox/repo/scripts" "$sandbox/bin"
cp "$REPO/scripts/build-fixtures-native.sh" "$sandbox/repo/scripts/"
cat > "$sandbox/repo/scripts/fixture-geometries.sh" <<'GEOM'
BTRFS_GEOMETRIES=("default:" "dup:-d dup -m dup")
BTRFS_POPULATED=()
BTRFS_FIXTURE_SIZE=1M
BTRFS_RICH_NAME=rich
BTRFS_RICH_SIZE=1M
BTRFS_RICH_MOUNT_OPTS=compress=zlib
GEOM

# mkfs.btrfs rejects the dup geometry when REJECT_DUP=1, as an older
# btrfs-progs or a runner without the feature would.
cat > "$sandbox/bin/mkfs.btrfs" <<'STUB'
#!/usr/bin/env bash
case " $* " in *" dup "*) [ "${REJECT_DUP:-0}" = 1 ] && exit 1 ;; esac
exit 0
STUB
printf '#!/usr/bin/env bash\necho "btrfs-progs v0 (stub)"\n' > "$sandbox/bin/btrfs"
printf '#!/usr/bin/env bash\nexit 0\n' > "$sandbox/bin/sudo"
chmod +x "$sandbox/bin/"*

# Each case states its whole environment: an override the developer
# already exported must not decide a case that did not ask for it.
run() {
    env -u BTRFS_FIXTURES_ALLOW_MISSING PATH="$sandbox/bin:$PATH" "$@" bash "$sandbox/repo/scripts/build-fixtures-native.sh" \
        > "$sandbox/out" 2>&1
}

expect() {
    local want="$1" got="$2" what="$3"
    if [ "$got" = "$want" ]; then
        printf 'ok    %s\n' "$what"
    else
        printf 'FAIL  %s: exit %s, expected %s\n' "$what" "$got" "$want"
        sed 's/^/      /' "$sandbox/out"
        fails=$((fails + 1))
    fi
}

# The control: every geometry builds, and the step passes.
run REJECT_DUP=0; expect 0 "$?" "every geometry built: the step passes"

# The defect: dup is rejected, default builds.
run REJECT_DUP=1; rc=$?
expect 1 "$([ "$rc" -ne 0 ] && echo 1 || echo 0)" "a geometry that did not build fails the step"
if grep -q "dup" "$sandbox/out"; then
    printf 'ok    and the failure names it\n'
else
    printf 'FAIL  the failure does not name the missing geometry\n'
    fails=$((fails + 1))
fi

# The escape hatch a developer uses, and only when asked for.
run REJECT_DUP=1 BTRFS_FIXTURES_ALLOW_MISSING=1
expect 0 "$?" "BTRFS_FIXTURES_ALLOW_MISSING=1 builds what it can and passes"

if [ "$fails" -eq 0 ]; then
    echo "PASS  fixture builder refuses a missing geometry"
else
    echo "FAIL  $fails check(s)" >&2
    exit 1
fi
