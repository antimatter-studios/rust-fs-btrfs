# fs.btrfs xattr, as installed: a path with no attributes lists none, in
# JSON and --text, and a missing attribute or path is refused by name with
# nothing on stdout, leaving the image as it was. Listing and reading values
# Linux set is tests/cli_xattr_kernel.rs; values this library set,
# tests/cli_xattr.rs.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/xattr.img"
mkfs.btrfs -q --text --size 128M "$img" >/dev/null 2>&1
check "mkfs.btrfs made the image" test -s "$img"
fs.btrfs "$img" create /f >/dev/null 2>&1
check "create /f exits 0" test $? -eq 0
cp "$img" "$SANDBOX/before.img"

fs.btrfs "$img" xattr /f >"$SANDBOX/list.json" 2>"$SANDBOX/list.err"
check "xattr /f exits 0 ($(cat "$SANDBOX/list.err"))" test $? -eq 0
jq_check "xattr /f lists no attributes" '. == []' "$SANDBOX/list.json"

fs.btrfs --text "$img" xattr /f >"$SANDBOX/list.txt" 2>"$SANDBOX/list.err"
check "xattr /f --text exits 0" test $? -eq 0

# refused STATUS ARGS... -- WHY: exit STATUS, nothing on stdout, and a
# structured error naming WHY.
refused() {
    local status="$1"
    shift
    local why="${*: -1}"
    local args=("${@:1:$#-2}")
    fs.btrfs "$img" "${args[@]}" >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
    check "${args[*]} exits $status" test $? -eq "$status"
    check "${args[*]} prints nothing on stdout" test ! -s "$SANDBOX/r.out"
    jq_check "${args[*]} is refused with code $status, naming $why" \
        ".code == $status and (.error | contains(\"$why\"))" \
        "$SANDBOX/r.err"
}

refused 1 xattr /f user.absent -- "user.absent"
refused 1 xattr /absent -- "/absent"

check "xattr left the image byte-identical" cmp -s "$img" "$SANDBOX/before.img"

finish
