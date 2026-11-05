#!/usr/bin/env bash
#
# fixture-pool-registry.sh — two fixture builds in one guest leave each other's
# pools alone (fs-linux-test-harness#157).
#
# THE DEVICE REGISTRY IS THE KERNEL'S, ONE FOR THE WHOLE GUEST. A multi-device
# pool mounts only when the kernel knows every member, and `mkfs.btrfs` or
# `btrfs device scan` is what tells it. `btrfs device scan --forget` with no
# device drops every member of every pool that is not mounted, whoever
# registered it. So on a shared guest one build's forget, between another's
# format and its mount, makes that mount fail for want of members.
#
# The stubs below model that rule and nothing else: a registry of devices, a
# forget that drops all unmounted ones or the ones named, and a pool mount that
# needs every member registered. They drive the real recipes, two at once, with
# the first build held between its pool's format and its mount. The real kernel
# runs the same recipes in `chore fixtures`.
#
#   bash tests/scripts/fixture-pool-registry.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RECIPES="$REPO/test-disks/guest-build-images.sh"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/fixture-pool-registry.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT
mkdir -p "$sandbox/bin"
export PRG_REAL_MKTEMP KERNEL="$sandbox/kernel" BTRFS_REGISTRY_DIR="$sandbox/registry" BTRFS_LOOP_DEV_DIR="$sandbox/kernel/dev"
PRG_REAL_MKTEMP="$(command -v mktemp)"

# --- the stubs -----------------------------------------------------------
#
# An image is a text file: "<fsid> <members> <devid>". $KERNEL/loops/<n> is
# what /dev/loop<n> is attached to, $KERNEL/reg/loop<n> the registry's entry
# for that device, and $KERNEL/mounted/<fsid> a mounted filesystem.
for tool in setfattr getfattr setfacl getfacl chattr lsattr python3 sync dd dmesg; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$sandbox/bin/$tool"
done
printf '#!/usr/bin/env bash\necho 0\n' > "$sandbox/bin/id"
printf '#!/usr/bin/env bash\necho 1\n' > "$sandbox/bin/od"
printf '#!/usr/bin/env bash\necho 0\n' > "$sandbox/bin/stat"
printf '#!/usr/bin/env bash\necho "0  -"\n' > "$sandbox/bin/sha256sum"
printf '#!/usr/bin/env bash\n[ "$1" = settle ]\n' > "$sandbox/bin/udevadm"
cat > "$sandbox/bin/mktemp" <<'STUB'
#!/usr/bin/env bash
exec "$PRG_REAL_MKTEMP" -d "$KERNEL/work.XXXXXX"
STUB
cat > "$sandbox/bin/truncate" <<'STUB'
#!/usr/bin/env bash
: > "${!#}"
STUB
cat > "$sandbox/bin/cp" <<'STUB'
#!/usr/bin/env bash
args=()
for arg in "$@"; do [ "$arg" = --sparse=always ] || args+=("$arg"); done
exec /bin/cp "${args[@]}"
STUB
cat > "$sandbox/bin/losetup" <<'STUB'
#!/usr/bin/env bash
mkdir -p "$KERNEL/loops"
case "$1" in
    --find)
        file="${!#}"
        exec 9>"$KERNEL/loops.lock"; perl -e 'use Fcntl ":flock"; open(my $f, ">&=", 9); flock($f, LOCK_EX)'
        n=0; while [ -e "$KERNEL/loops/$n" ]; do n=$((n + 1)); done
        echo "$file" > "$KERNEL/loops/$n"
        mkdir -p "$KERNEL/dev"; : > "$KERNEL/dev/loop$n"
        echo "/dev/loop$n"
        ;;
    -d) rm -f "$KERNEL/loops/${2#/dev/loop}" ;;
    -a) for f in "$KERNEL"/loops/*; do [ -f "$f" ] && echo "/dev/loop${f##*/}: [0]:0 ($(cat "$f"))"; done ;;
    *) echo "/dev/loop0: 0 0 (the stub loop device listing)" ;;
esac
exit 0
STUB
cat > "$sandbox/bin/mkfs.btrfs" <<'STUB'
#!/usr/bin/env bash
devices=()
for arg in "$@"; do
    case "$arg" in /dev/loop* | /*) devices+=("$arg") ;; esac
done
mkdir -p "$KERNEL/reg"
exec 9>"$KERNEL/fsid.lock"; perl -e 'use Fcntl ":flock"; open(my $f, ">&=", 9); flock($f, LOCK_EX)'
fsid="fs$(( $(cat "$KERNEL/fsid" 2>/dev/null || echo 0) + 1 ))"
echo "${fsid#fs}" > "$KERNEL/fsid"
i=0
for dev in "${devices[@]}"; do
    i=$((i + 1))
    case "$dev" in
        /dev/loop*)
            echo "$fsid ${#devices[@]} $i" > "$(cat "$KERNEL/loops/${dev#/dev/loop}")"
            echo "$fsid" > "$KERNEL/reg/${dev#/dev/}"
            ;;
        *) echo "$fsid ${#devices[@]} $i" > "$dev" ;;
    esac
done
STUB
cat > "$sandbox/bin/btrfs" <<'STUB'
#!/usr/bin/env bash
mkdir -p "$KERNEL/reg" "$KERNEL/mounted"
case "$1 $2 ${3:-}" in
    "--version  ") echo "btrfs-progs v0 (stub)" ;;
    "filesystem df "*) printf 'Data, RAID0: x\nData, RAID1: x\nData, RAID5: x\nData, RAID6: x\n' ;;
    "device scan --forget")
        shift 3
        if [ $# -eq 0 ]; then
            for r in "$KERNEL"/reg/*; do
                [ -f "$r" ] || continue
                [ -e "$KERNEL/mounted/$(cat "$r")" ] || rm -f "$r"
            done
        else
            for dev in "$@"; do
                r="$KERNEL/reg/${dev##*/}"
                [ -f "$r" ] && [ ! -e "$KERNEL/mounted/$(cat "$r")" ] && rm -f "$r"
            done
        fi
        ;;
    "device scan "*)
        shift 2
        for dev in "$@"; do
            read -r fsid _ < "$(cat "$KERNEL/loops/${dev#/dev/loop}")"
            echo "$fsid" > "$KERNEL/reg/${dev#/dev/}"
        done
        ;;
esac
exit 0
STUB
cat > "$sandbox/bin/mount" <<'STUB'
#!/usr/bin/env bash
case " $* " in *" remount"*|*"remount,"*) exit 0 ;; esac
source="${*: -2:1}"
target="${!#}"
case "$source" in
    /dev/loop*) image="$(cat "$KERNEL/loops/${source#/dev/loop}")" ;;
    *) image="$source" ;;
esac
read -r fsid members _ < "$image"
if [ "${members:-1}" -gt 1 ]; then
    if [ -e "$KERNEL/gate" ] && [ ! -e "$KERNEL/gate.reached" ]; then
        : > "$KERNEL/gate.reached"
        while [ -e "$KERNEL/gate" ]; do sleep 0.1; done
    fi
    known=$(grep -lx "$fsid" "$KERNEL"/reg/* 2>/dev/null | wc -l | tr -d ' ')
    if [ "$known" -lt "$members" ]; then
        echo "BTRFS error: devid missing for $fsid, $known of $members known" >> "$KERNEL/dmesg.log"
        echo "mount: $target: wrong fs type, bad option, bad superblock on $source, missing codepage or helper program, or other error." >&2
        exit 32
    fi
fi
mkdir -p "$KERNEL/mounted"
echo "$target" > "$KERNEL/mounted/$fsid"
STUB
cat > "$sandbox/bin/umount" <<'STUB'
#!/usr/bin/env bash
for m in "$KERNEL"/mounted/*; do
    [ -f "$m" ] && [ "$(cat "$m")" = "$1" ] && rm -f "$m"
done
exit 0
STUB
cat > "$sandbox/bin/mountpoint" <<'STUB'
#!/usr/bin/env bash
grep -qxF "${!#}" "$KERNEL"/mounted/* 2>/dev/null
STUB
# flock(1) on a descriptor: perl takes the lock on the open file description
# that this process inherited, so it holds for the shell that opened it.
cat > "$sandbox/bin/flock" <<'STUB'
#!/usr/bin/env bash
mode=LOCK_EX
case "$1" in -n) mode="LOCK_EX|LOCK_NB"; shift ;; -u) mode=LOCK_UN; shift ;; esac
exec perl -e 'use Fcntl ":flock"; open(my $f, ">&=", $ARGV[0]) or exit 2; flock($f, '"$mode"') or exit 1' "$1"
STUB
chmod +x "$sandbox"/bin/*

pid_first="" pid_second=""
start() { # start <name> <target>: a build in the background, in $pid_<name>
    mkdir -p "$sandbox/out-$1"
    ( PATH="$sandbox/bin:$PATH" exec bash "$RECIPES" "$sandbox/out-$1" "$2" ) > "$sandbox/$1.log" 2>&1 &
    eval "pid_$1=\$!"
}
finish() { # finish <name>: its status in $rc, bounded
    local pid n=0
    pid="$(eval echo "\$pid_$1")"
    while kill -0 "$pid" 2>/dev/null && [ "$n" -lt 600 ]; do sleep 0.1; n=$((n + 1)); done
    if kill -0 "$pid" 2>/dev/null; then kill -KILL "$pid" 2>/dev/null; { wait "$pid"; } 2>/dev/null; rc=hung; return; fi
    { wait "$pid"; } 2>/dev/null
    rc=$?
}
reach_gate() {
    local n=0
    while [ ! -e "$KERNEL/gate.reached" ] && [ "$n" -lt 600 ]; do sleep 0.1; n=$((n + 1)); done
    [ -e "$KERNEL/gate.reached" ]
}
fresh() { rm -rf "$KERNEL" "$BTRFS_REGISTRY_DIR" "$sandbox"/out-* "$sandbox"/*.log; mkdir -p "$KERNEL"; }

# --- 1. Another build's forget does not take a pool's members. -----------
fresh
: > "$KERNEL/gate"
start first pool
reach_gate || fail "the first build never reached its pool's mount ($(tail -n 3 "$sandbox/first.log"))"
start second cow
sleep 3
rm -f "$KERNEL/gate"
finish first
first_rc=$rc
finish second
[ "$first_rc" = 0 ] && ok "a pool mounts with another build running beside it" \
    || fail "a pool mounts with another build running beside it (status $first_rc): $(grep -h 'BTRFS\|mount:' "$sandbox/first.log" "$KERNEL/dmesg.log" 2>/dev/null | head -n 2)"
[ "$rc" = 0 ] && ok "and the other build finishes too" || fail "and the other build finishes too (status $rc): $(tail -n 2 "$sandbox/second.log")"

# --- 2. A second pool waits for the first, and says for whom. ------------
fresh
: > "$KERNEL/gate"
start first pool
reach_gate || fail "the first build never reached its pool's mount"
start second pool
sleep 3
registered="$(find "$KERNEL/reg" -type f 2>/dev/null | wc -l | tr -d ' ')"
if kill -0 "$pid_second" 2>/dev/null && [ "$registered" = 2 ]; then
    ok "a second pool waits, unformatted, while the first is between its format and its unmount"
else
    fail "a second pool waits, unformatted, while the first is between its format and its unmount ($registered devices registered; $(tail -n 1 "$sandbox/second.log"))"
fi
case "$(cat "$sandbox/second.log")" in
    *"waiting for the kernel's device registry, held by pid $pid_first"*) ok "naming who holds it" ;;
    *) fail "naming who holds it (got: $(grep -i wait "$sandbox/second.log" | head -n 1))" ;;
esac
rm -f "$KERNEL/gate"
finish first
first_rc=$rc
finish second
[ "$first_rc" = 0 ] && [ "$rc" = 0 ] && ok "and both pools are built" \
    || fail "and both pools are built (statuses $first_rc and $rc): $(tail -n 3 "$sandbox/first.log"; tail -n 3 "$sandbox/second.log")"
left="$(find "$KERNEL/reg" -type f 2>/dev/null)"
[ -z "$left" ] && ok "with nothing of theirs left in the registry" \
    || fail "with nothing of theirs left in the registry: $left"

# --- 3. No recipe forgets every device. ----------------------------------
found="$(grep -nE 'btrfs device scan --forget *(>|2>|\|\||$)' "$RECIPES")"
[ -z "$found" ] && ok "every forget in the recipes names its devices" \
    || fail "every forget in the recipes names its devices: $found"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails fixture-pool-registry violation(s)" >&2
    exit 1
fi
echo "PASS  two fixture builds in one guest leave each other's pools alone"
