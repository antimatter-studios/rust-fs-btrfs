# What fs.btrfs refuses before it opens the image answers by name: status
# 3 for a key that cannot be set, status 2 for an unknown key or a size
# `resize` cannot read, a structured error naming why, and nothing on
# stdout. A missing target proves these refusals happen before opening it;
# positioned writes also leave a copy of a kernel-made image byte-identical.
#
# `resize` was this file's verb that answered `not implemented` until it
# could resize (#264); what it does to a volume is checked in
# tests/cli_resize_kernel.rs, in the harness VM.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/never-opened.img"

# refused STATUS VERB... -- WHY: exit STATUS, nothing on stdout, and a
# structured error with that code naming WHY.
refused() {
    local status="$1"
    shift
    local why="${*: -1}"
    local args=("${@:1:$#-2}")
    fs.btrfs "$img" "${args[@]}" >"$SANDBOX/b.out" 2>"$SANDBOX/b.err"
    check "${args[*]} exits $status" test $? -eq "$status"
    check "${args[*]} prints nothing on stdout" test ! -s "$SANDBOX/b.out"
    jq_check "${args[*]} is refused with code $status, naming $why" \
        ".code == $status and (.error | contains(\"$why\"))" \
        "$SANDBOX/b.err"
}

# A read-only key is refused with status 3; an unknown one is a wrong
# command line.
refused 3 set total_bytes 1 -- "read-only"
refused 2 set colour blue -- "colour"

# A size resize cannot read is a wrong command line, whatever --force says.
refused 2 resize 20Q -- "not a size"
refused 2 resize 0 --force -- "not a size"
refused 2 resize -x -- "not a size"

# What balance cannot read is a wrong command line, and a kernel filter it
# does not implement says so; both before the image is opened (#402).
refused 2 balance -- "-d, -m or both"
refused 2 balance -dconvert=raid7 -- "not a profile"
refused 2 balance -dusage=101 -- "percentage"
refused 2 balance -mprofiles=single\|mirror -- "not a profile"
refused 2 balance -dfrobnicate -- "not a balance filter"
refused 3 balance -dlimit=2 -- "not implemented: the balance filter limit"

# Positioned writes are a missing capability, including position zero.
# A missing image proves refusal happens before opening the target.
refused 3 write --at 0 /f -- "not implemented: positioned write"
refused 3 write --at 18446744073709551615 /f -- "positioned write"
check "write --at does not create the missing image" test ! -e "$img"
refused 2 write --at nope /f -- "nope"
refused 2 write --at 18446744073709551616 /f -- "18446744073709551616"

# Refusal also leaves a real kernel-made volume byte-identical.
src="$REPO/test-disks/cli/btrfs-cli.img"
if [ ! -s "$src" ]; then
    fail "test-disks/cli/btrfs-cli.img is missing: run chore fixtures in the harness VM. Nothing skips."
    finish
fi
img="$SANDBOX/positioned-write.img"
dd if="$src" of="$img" bs=1048576 conv=sparse status=none 2>/dev/null ||
    dd if="$src" of="$img" bs=1048576 conv=sparse 2>/dev/null
refused 3 write --at 0 /f -- "not implemented: positioned write"
check "write --at 0 leaves the image byte-identical" cmp -s "$src" "$img"
refused 3 --offset 1048576 write --at 17 /nocow/data.bin -- "positioned write"
check "--offset and --at leave the image byte-identical" cmp -s "$src" "$img"

# --text uses the same refusal and never reports success on stdout.
fs.btrfs --text "$img" write --at 0 /f >"$SANDBOX/t.out" 2>"$SANDBOX/t.err"
check "write --at --text exits 3" test $? -eq 3
check "write --at --text prints nothing on stdout" test ! -s "$SANDBOX/t.out"
check "write --at --text names the missing capability" grep -q '^fs.btrfs: not implemented: positioned write' "$SANDBOX/t.err"
check "write --at --text leaves the image byte-identical" cmp -s "$src" "$img"

# --text: `fs.btrfs: <message>`, for a person.
fs.btrfs --text "$img" set total_bytes 1 >/dev/null 2>"$SANDBOX/t.err"
check "set total_bytes --text exits 3" test $? -eq 3
check "set total_bytes --text says so for a person" grep -q '^fs.btrfs: total_bytes is read-only' "$SANDBOX/t.err"

finish
