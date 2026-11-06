#!/usr/bin/env bash
#
# fixture-copy-mount.sh — a copy of a fixture image is mounted only after
# the kernel has forgotten the filesystem it was copied from, and a mount
# that fails says what the kernel said (#736).
#
# A COPY HAS THE SAME FILESYSTEM IDENTIFIER AS ITS ORIGINAL, and the kernel
# keeps a record of every identifier it has scanned, after the unmount too.
# Scanning the same identifier and device number at a LOWER generation
# through a DIFFERENT device path is refused with EEXIST:
#
#   mount: <dir>: mount(2) system call failed: File exists.
#   BTRFS error: device /dev/loop1 already registered with a higher
#   generation, found 8 expect 9
#
# Measured on the guest's Linux 6.1. `build_cow` mounts `control` and then
# `after`, both copies of `before`, and mounting `control` advances its
# generation, so `after` is the older of the two. It usually mounts anyway.
# It fails only when both of these hold, which is what made this a flake:
#
#   - udev's `btrfs ready` scan of the device, queued when the loop device
#     was attached, runs after the unmount, so the record holds the newer
#     generation. A scan made while the filesystem is mounted changes nothing.
#   - that udev worker still holds the device open, so the auto-clearing loop
#     device is not free yet and the next `mount -o loop` takes another.
#
# THE ORDER IN `mount_copy` IS THE FIX. `udevadm settle` first, because a
# forget that runs ahead of the worker is undone when the worker's scan
# lands. Once the worker is done the loop device has detached, so nothing can
# scan the old image again, and only then does the forget hold.
#
# The stubs below model that one kernel rule at its worst: every unmount is
# followed by the late scan, and every mount of an older copy is refused
# while the record stands. That makes a timing failure fail every time, with
# no VM, no root and no btrfs-progs. The real kernel runs the same recipe in
# `chore fixtures`.
#
#   bash tests/scripts/fixture-copy-mount.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RECIPES="$REPO/test-disks/guest-build-images.sh"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/fixture-copy-mount.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT
mkdir -p "$sandbox/bin"

export FCM_REAL_CP FCM_REAL_MKTEMP KERNEL="$sandbox/kernel" BTRFS_REGISTRY_DIR="$sandbox/registry" BTRFS_LOOP_DEV_DIR="$sandbox/dev"
# One loop device node, detached, which is where a copy's earlier mount left its record.
mkdir -p "$sandbox/dev"
: > "$sandbox/dev/loop1"
FCM_REAL_CP="$(command -v cp)"
FCM_REAL_MKTEMP="$(command -v mktemp)"

# --- the stubs -----------------------------------------------------------
#
# An image is a text file holding its filesystem identifier and generation,
# so a copy shares the first and starts level on the second. $KERNEL/<id>
# is the kernel's record of that identifier.
for tool in setfattr getfattr setfacl getfacl chattr lsattr python3 sha256sum dd sync; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$sandbox/bin/$tool"
done
printf '#!/usr/bin/env bash\necho 0\n' > "$sandbox/bin/id"
printf '#!/usr/bin/env bash\necho 0\n' > "$sandbox/bin/od"
cat > "$sandbox/bin/mktemp" <<'STUB'
#!/usr/bin/env bash
exec "$FCM_REAL_MKTEMP" -d "$KERNEL/work.XXXXXX"
STUB
cat > "$sandbox/bin/truncate" <<'STUB'
#!/usr/bin/env bash
: > "${!#}"
STUB
cat > "$sandbox/bin/cp" <<'STUB'
#!/usr/bin/env bash
args=()
for arg in "$@"; do
    [ "$arg" = --sparse=always ] || args+=("$arg")
done
exec "$FCM_REAL_CP" "${args[@]}"
STUB
cat > "$sandbox/bin/mkfs.btrfs" <<'STUB'
#!/usr/bin/env bash
count=$(( $(cat "$KERNEL/count" 2>/dev/null || echo 0) + 1 ))
echo "$count" > "$KERNEL/count"
echo "fs$count 6" > "${!#}"
STUB
cat > "$sandbox/bin/mount" <<'STUB'
#!/usr/bin/env bash
image="${*: -2:1}"
target="${!#}"
refuse() {
    echo "$1" >> "$KERNEL/dmesg"
    echo "mount: $target: mount(2) system call failed: File exists." >&2
    exit 32
}
[ -z "${STUB_MOUNT_REFUSE:-}" ] || refuse "BTRFS error: the stub kernel refused this"
read -r id generation < "$image"
known=0
[ ! -f "$KERNEL/$id" ] || known="$(cat "$KERNEL/$id")"
[ "$generation" -ge "$known" ] ||
    refuse "BTRFS error: device /dev/loop1 already registered with a higher generation, found $generation expect $known"
echo "$image" > "$KERNEL/mounted"
STUB
cat > "$sandbox/bin/umount" <<'STUB'
#!/usr/bin/env bash
image="$(cat "$KERNEL/mounted")"
read -r id generation < "$image"
generation=$(( generation + 1 ))
echo "$id $generation" > "$image"
rm -f "$KERNEL/mounted"
# udev's late scan, and the worker that has not finished yet.
echo "$generation" > "$KERNEL/$id"
touch "$KERNEL/worker-running"
STUB
cat > "$sandbox/bin/mountpoint" <<'STUB'
#!/usr/bin/env bash
[ -f "$KERNEL/mounted" ]
STUB
cat > "$sandbox/bin/udevadm" <<'STUB'
#!/usr/bin/env bash
[ "$1" = settle ] || exit 2
rm -f "$KERNEL/worker-running"
STUB
cat > "$sandbox/bin/btrfs" <<'STUB'
#!/usr/bin/env bash
case "$*" in
    --version) echo "btrfs-progs v0 (stub)" ;;
    "device scan --forget"*)
        # A forget ahead of the worker is undone by the worker's scan. The model
        # has one device, so forgetting it forgets every record.
        [ -f "$KERNEL/worker-running" ] || find "$KERNEL" -name 'fs[0-9]*' -type f -delete
        ;;
esac
exit 0
STUB
cat > "$sandbox/bin/dmesg" <<'STUB'
#!/usr/bin/env bash
cat "$KERNEL/dmesg" 2>/dev/null
STUB
cat > "$sandbox/bin/losetup" <<'STUB'
#!/usr/bin/env bash
echo "/dev/loop0: 0 0 (the stub loop device listing)"
STUB
cat > "$sandbox/bin/flock" <<'STUB'
#!/usr/bin/env bash
mode=LOCK_EX
case "$1" in -n) mode="LOCK_EX|LOCK_NB"; shift ;; -u) mode=LOCK_UN; shift ;; esac
exec perl -e 'use Fcntl ":flock"; open(my $f, ">&=", $ARGV[0]) or exit 2; flock($f, '"$mode"') or exit 1' "$1"
STUB
chmod +x "$sandbox"/bin/*

# run <recipe file> <target>: a fresh kernel, a fresh output, $out and $rc.
run() {
    rm -rf "$KERNEL" "$sandbox/out"
    mkdir -p "$KERNEL" "$sandbox/out"
    out="$(PATH="$sandbox/bin:$PATH" bash "$1" "$sandbox/out" "$2" 2>&1)"
    rc=$?
}

# without <line>: the recipes with exactly that line removed, in $mutant.
without() {
    mutant="$sandbox/mutant.sh"
    grep -vxF "$1" "$RECIPES" > "$mutant"
    if cmp -s "$RECIPES" "$mutant"; then
        fail "the recipes hold the line '$1'"
    fi
}

# unguarded <file>: every plain `mount` of a file the same function copied.
unguarded() {
    awk '
        /^[a-z_]+\(\) *\{/ { split("", copies) }
        /^[ \t]*cp( --sparse=always)? "[^"]+" "[^"]+"/ {
            split($0, part, "\"")
            copies[part[4]] = 1
        }
        /^[ \t]*mount / {
            for (copy in copies) {
                if (index($0, "\"" copy "\"")) print FILENAME ":" FNR ":" $0
            }
        }
    ' "$1"
}

# --- 1. The cow recipe survives the late scan. ---------------------------
run "$RECIPES" cow
if [ "$rc" = 0 ]; then
    ok "the cow recipe mounts its copies with a newer generation on record"
else
    fail "the cow recipe mounts its copies with a newer generation on record (status $rc): $out"
fi
images=$(find "$sandbox/out" -name '*.img' | wc -l | tr -d ' ')
[ "$images" = 6 ] && ok "the cow recipe publishes its six images" \
                  || fail "the cow recipe publishes its six images (got $images)"

# --- 2. Each half of the fix is needed, so this guard can fail. ----------
without '    udevadm settle'
run "$mutant" cow
case "$rc:$out" in
    32:*"File exists"*) ok "a forget ahead of the udev worker does not hold" ;;
    *) fail "a forget ahead of the udev worker does not hold (status $rc)" ;;
esac
without '    forget_detached'
run "$mutant" cow
case "$rc:$out" in
    32:*"File exists"*) ok "a copy mounted with no forget is refused" ;;
    *) fail "a copy mounted with no forget is refused (status $rc)" ;;
esac

# --- 3. Every recipe, not only the one that failed. ----------------------
found="$(unguarded "$RECIPES")"
[ -z "$found" ] && ok "every mount of a copied image goes through mount_copy" \
                || fail "every mount of a copied image goes through mount_copy: $found"
cp "$RECIPES" "$sandbox/planted.sh"
cat >> "$sandbox/planted.sh" <<'PLANTED'
build_planted() {
    cp --sparse=always "$img" "$copy"
    mount -o loop "$copy" "$MNT"
}
PLANTED
found="$(unguarded "$sandbox/planted.sh" | grep -c 'mount -o loop "$copy"')"
[ "$found" = 1 ] && ok "a new recipe that mounts a copy directly is caught" \
                 || fail "a new recipe that mounts a copy directly is caught (found $found)"
grep -qE '^for tool in .* udevadm[ ;]' "$RECIPES" \
    && ok "the recipes name udevadm among the tools they need up front" \
    || fail "the recipes name udevadm among the tools they need up front"

# --- 4. A mount that fails says what the kernel said. --------------------
export STUB_MOUNT_REFUSE=1
run "$RECIPES" commit
unset STUB_MOUNT_REFUSE
[ "$rc" = 32 ] && ok "a failed mount keeps mount's own status" \
               || fail "a failed mount keeps mount's own status (got $rc)"
case "$out" in
    *"the stub kernel refused this"*) ok "a failed mount prints the kernel's log" ;;
    *) fail "a failed mount prints the kernel's log: $out" ;;
esac
case "$out" in
    *"the stub loop device listing"*) ok "a failed mount prints the loop devices" ;;
    *) fail "a failed mount prints the loop devices: $out" ;;
esac

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails fixture-copy-mount violation(s)" >&2
    exit 1
fi
echo "PASS  a copied image mounts after a settled forget, and a failed mount says why"
