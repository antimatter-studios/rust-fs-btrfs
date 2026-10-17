# The installed recursive listing contract; the independent kernel tree,
# subvolumes and snapshots are in cli_ls_recursive_kernel.rs.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/tree.img"
mkfs.btrfs -q --text --size 128M "$img" >/dev/null 2>&1
check "mkfs made the image" test -s "$img"
for dir in /a /a/b /a/b/c; do
    fs.btrfs "$img" mkdir "$dir" >/dev/null 2>&1
    check "mkdir $dir exits 0" test $? -eq 0
done
for file in /a/b/c/deep /a/f /z; do
    fs.btrfs "$img" create "$file" >/dev/null 2>&1
    check "create $file exits 0" test $? -eq 0
done
cp "$img" "$SANDBOX/before.img"
order='["/a","/a/b","/a/b/c","/a/b/c/deep","/a/f","/z"]'
fs.btrfs "$img" ls --recursive / >"$SANDBOX/tree.json" 2>"$SANDBOX/tree.err"
check "recursive ls exits 0" test $? -eq 0
check "a complete walk prints no error" test ! -s "$SANDBOX/tree.err"
jq_check "depth first, names sorted as bytes" "[.[].path] == $order" "$SANDBOX/tree.json"
jq_check "depth counts from PATH" '[.[].depth] == [1,2,3,4,2,1]' "$SANDBOX/tree.json"
jq_check "ls fields plus path and depth" \
    'all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number"
    and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number"
    and .subvolume == false and (.path|type)=="string" and (.depth|type)=="number" and (has("revisited")|not))' "$SANDBOX/tree.json"
fs.btrfs "$img" ls / >"$SANDBOX/root.json" 2>/dev/null
check "depth 1 is plain ls" test "$(jq -c '[.[]|select(.depth==1)|del(.path,.depth)]' "$SANDBOX/tree.json")" = "$(jq -c . "$SANDBOX/root.json")"
jq_check "plain ls has no recursive fields" 'all(.[]; (has("path") or has("depth") or has("revisited"))|not)' "$SANDBOX/root.json"
fs.btrfs "$img" ls -R / >"$SANDBOX/again.json" 2>/dev/null
check "-R exits 0" test $? -eq 0
check "two walks print identical bytes" cmp -s "$SANDBOX/tree.json" "$SANDBOX/again.json"
fs.btrfs "$img" ls -R --max-depth 4 --limit 6 / >"$SANDBOX/fit.json" 2>/dev/null
check "an exact fit succeeds" test $? -eq 0
check "an exact fit keeps every entry" cmp -s "$SANDBOX/tree.json" "$SANDBOX/fit.json"
for spec in 'max-depth 3 3 /a/b/c/deep' 'limit 4 4 /a/f' 'max-depth 0 0 /a' 'limit 0 0 /a'; do
    read -r bound value count stopped <<<"$spec"
    fs.btrfs "$img" ls -R "--$bound" "$value" / >"$SANDBOX/partial.json" 2>"$SANDBOX/partial.err"
    check "--$bound $value exits 1" test $? -eq 1
    check "--$bound keeps the prefix" test "$(jq -c . "$SANDBOX/partial.json")" = "$(jq -c ".[0:$count]" "$SANDBOX/tree.json")"
    jq_check "the error names the bound and stopping path" \
        ".code==1 and (.error|contains(\"--$bound $value\")) and (.error|contains(\"$stopped\"))" "$SANDBOX/partial.err"
done
fs.btrfs "$img" ls -R a//b/ >"$SANDBOX/sub.json" 2>/dev/null
check "a relative subtree succeeds" test $? -eq 0
jq_check "absolute paths, relative depths" '[.[]|[.path,.depth]]==[["/a/b/c",1],["/a/b/c/deep",2]]' "$SANDBOX/sub.json"
fs.btrfs "$img" ls -R /z >"$SANDBOX/file.json" 2>/dev/null
check "a starting file succeeds" test $? -eq 0
jq_check "a starting file is depth 0" 'length==1 and .[0].path=="/z" and .[0].depth==0 and .[0].type=="file"' "$SANDBOX/file.json"
fs.btrfs "$img" ls -R --limit 0 /z >"$SANDBOX/zero.json" 2>"$SANDBOX/zero.err"
check "zero limit at a file exits 1" test $? -eq 1
jq_check "zero limit keeps an empty array" '.==[]' "$SANDBOX/zero.json"
fs.btrfs "$img" ls -R --text / >"$SANDBOX/tree.txt" 2>/dev/null
check "text walk succeeds" test $? -eq 0
check "text has one line per entry" test "$(wc -l <"$SANDBOX/tree.txt")" -eq 6
check "text uses the full path" grep -q ' /a/b/c/deep$' "$SANDBOX/tree.txt"
fs.btrfs --text "$img" ls -R --json --limit 1 / >"$SANDBOX/format.json" 2>"$SANDBOX/format.err"
check "last format wins on a bound" test $? -eq 1
jq_check "partial report is JSON" 'length==1' "$SANDBOX/format.json"
jq_check "error is JSON" '.code==1' "$SANDBOX/format.err"
fs.btrfs "$img" ls -R --text --limit 2 / >"$SANDBOX/partial.txt" 2>"$SANDBOX/text.err"
check "text bound exits 1" test $? -eq 1
check "text keeps its prefix" test "$(wc -l <"$SANDBOX/partial.txt")" -eq 2
for args in '--max-depth 2' '--limit 2' '-R --limit nope' '-R --max-depth -1'; do
    # shellcheck disable=SC2086 # command-line words under test
    fs.btrfs "$img" ls $args / >"$SANDBOX/usage.out" 2>"$SANDBOX/usage.err"
    check "$args is a usage error" test $? -eq 2
    check "usage error has no stdout" test ! -s "$SANDBOX/usage.out"
    jq_check "structured usage error" '.code==2' "$SANDBOX/usage.err"
done
fs.btrfs "$img" ls -R /missing >"$SANDBOX/missing.out" 2>"$SANDBOX/missing.err"
check "missing start exits 1" test $? -eq 1
check "missing start has no report" test ! -s "$SANDBOX/missing.out"
jq_check "missing start says no such file" '.code==1 and (.error|contains("no such file"))' "$SANDBOX/missing.err"
{ head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/offset.img"
fs.btrfs --offset 1048576 "$SANDBOX/offset.img" ls -R / >"$SANDBOX/offset.json" 2>/dev/null
check "offset walk succeeds" test $? -eq 0
check "offset keeps the listing" cmp -s "$SANDBOX/tree.json" "$SANDBOX/offset.json"
check "every read and refusal leaves the image unchanged" cmp -s "$img" "$SANDBOX/before.img"
finish
