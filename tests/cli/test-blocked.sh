# What fs.btrfs refuses before it opens the image answers by name: status
# 3 for a key that cannot be set, status 2 for an unknown key or a size
# `resize` cannot read, a structured error naming why, and nothing on
# stdout. None of them opens the image, so none needs one.
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

# A positioned write, `write --at BYTES`, is what the other writable tools
# take for a write inside a file; this driver has none, so it answers
# status 3, not implemented, before stdin or the image is read (#415).
refused 3 write --at 0 /f -- "not implemented" </dev/null
check "write --at leaves no image behind" test ! -e "$img"

# --text: `fs.btrfs: <message>`, for a person.
fs.btrfs --text "$img" set total_bytes 1 >/dev/null 2>"$SANDBOX/t.err"
check "set total_bytes --text exits 3" test $? -eq 3
check "set total_bytes --text says so for a person" grep -q '^fs.btrfs: total_bytes is read-only' "$SANDBOX/t.err"

finish
