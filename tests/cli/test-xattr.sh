# Installed xattr inspection against attributes set by the guest kernel.
source "$(dirname "$0")/lib.sh"

img="$REPO/test-disks/btrfs-xattr.img"
if [ ! -s "$img" ]; then
    fail "btrfs-xattr.img is missing: run chore fixtures in the harness VM"
    finish
fi

fs.btrfs "$img" xattr /bare.txt >"$SANDBOX/none.json" 2>"$SANDBOX/err"
check "a file with no attributes succeeds" test $? -eq 0
jq_check "no attributes is []" '. == []' "$SANDBOX/none.json"
fs.btrfs "$img" xattr /plain.txt >"$SANDBOX/list.json" 2>"$SANDBOX/err"
check "listing succeeds" test $? -eq 0
jq_check "names retain namespace prefixes and sizes are numbers" \
    'length > 0 and all(.[]; (.name|type)=="string" and (.size|type)=="number") and any(.[]; .name=="user.binary" and .size==7) and any(.[]; .name=="user.empty" and .size==0)' \
    "$SANDBOX/list.json"
fs.btrfs "$img" xattr /plain.txt --text >"$SANDBOX/list.txt" 2>"$SANDBOX/err"
check "text listing succeeds" test $? -eq 0
check "text listing shows the attribute and length" grep -q 'user.binary.*7' "$SANDBOX/list.txt"
printf '\000\001\002\377\177\012\000' >"$SANDBOX/want"
fs.btrfs "$img" xattr /plain.txt user.binary --text >"$SANDBOX/value" 2>"$SANDBOX/err"
check "a value succeeds even with --text" test $? -eq 0
check "a value is raw binary" cmp -s "$SANDBOX/value" "$SANDBOX/want"
fs.btrfs "$img" xattr /plain.txt user.binary -o "$SANDBOX/saved" >"$SANDBOX/out" 2>"$SANDBOX/err"
check "-o succeeds" test $? -eq 0
check "-o prints nothing" test ! -s "$SANDBOX/out"
check "-o preserves binary bytes" cmp -s "$SANDBOX/saved" "$SANDBOX/want"
check "-o leaves no partial file" test ! -e "$SANDBOX/saved.partial"
fs.btrfs "$img" xattr /plain.txt user.empty >"$SANDBOX/empty" 2>"$SANDBOX/err"
check "an empty value succeeds" test $? -eq 0
check "an empty value has zero bytes" test ! -s "$SANDBOX/empty"

cp "$img" "$SANDBOX/before.img"
printf 'keep me' >"$SANDBOX/saved"
fs.btrfs "$img" xattr /plain.txt user.never-set -o "$SANDBOX/saved" >"$SANDBOX/out" 2>"$SANDBOX/err"
check "a missing attribute exits 1" test $? -eq 1
check "a missing attribute has no stdout" test ! -s "$SANDBOX/out"
jq_check "a missing attribute names it" '.code==1 and (.error|contains("user.never-set"))' "$SANDBOX/err"
check "a missing attribute preserves the output file" test "$(cat "$SANDBOX/saved")" = 'keep me'
fs.btrfs "$img" xattr /no-such-path >"$SANDBOX/out" 2>"$SANDBOX/err"
check "a missing path exits 1" test $? -eq 1
check "a missing path has no stdout" test ! -s "$SANDBOX/out"
jq_check "a missing path is named" '.code==1 and (.error|contains("no-such-path"))' "$SANDBOX/err"
check "inspection and refusals leave the image unchanged" cmp -s "$img" "$SANDBOX/before.img"

dd if="$img" of="$SANDBOX/whole.img" bs=1048576 seek=1 conv=sparse,notrunc 2>"$SANDBOX/dd.err"
fs.btrfs --offset 1048576 "$SANDBOX/whole.img" xattr /plain.txt user.binary >"$SANDBOX/offset" 2>"$SANDBOX/err"
check "--offset remains the filesystem's device offset" test $? -eq 0
check "--offset reads the same raw value" cmp -s "$SANDBOX/offset" "$SANDBOX/want"

finish
