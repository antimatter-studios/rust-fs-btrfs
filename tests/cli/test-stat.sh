# fs.btrfs stat, as installed: the shared keys in the shared order and
# typed, Btrfs's own under `btrfs`, the values `create`, `mkdir` and `ln` produced,
# --text, the refusals, and an image left exactly as it was. What the
# values must be on a volume Linux wrote is tests/cli_stat_kernel.rs.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/stat.img"
mkfs.btrfs -q --text --size 128M "$img" >/dev/null 2>&1
check "mkfs.btrfs made the image" test -s "$img"
fs.btrfs "$img" mkdir /d >/dev/null 2>&1
check "mkdir /d exits 0" test $? -eq 0
fs.btrfs "$img" create /f >/dev/null 2>&1
check "create /f exits 0" test $? -eq 0
fs.btrfs "$img" ln -s f /link >/dev/null 2>&1
check "ln -s f /link exits 0" test $? -eq 0
raw=$'/raw-\xff'
fs.btrfs "$img" create "$raw" >/dev/null 2>&1
check "create a non-UTF-8 path exits 0" test $? -eq 0
cp "$img" "$SANDBOX/before.img"

shared='["path","inode","type","mode","uid","gid","nlink","size","allocated_bytes","atime","atime_nsec","mtime","mtime_nsec","ctime","ctime_nsec","btime","btime_nsec"]'
typed='(.path|type)=="string" and (.inode|type)=="number" and (.type|type)=="string"
    and (.mode|test("^[0-7]{4}$")) and (.uid|type)=="number" and (.gid|type)=="number"
    and (.nlink|type)=="number" and (.size|type)=="number" and (.allocated_bytes|type)=="number"
    and (.atime|type)=="number" and (.mtime|type)=="number" and (.ctime|type)=="number"
    and all(.atime_nsec, .mtime_nsec, .ctime_nsec, .btime, .btime_nsec; type=="number" or type=="null")
    and (.btrfs|type)=="object" and (.btrfs.flags|type)=="array"
    and all(.btrfs.subvolume, .btrfs.generation, .btrfs.transid, .btrfs.sequence; type=="number")
    and (.btrfs|has("compression")) and (.btrfs.compression == null)'

for path in / /d /f /link; do
    fs.btrfs "$img" stat "$path" >"$SANDBOX/stat.json" 2>"$SANDBOX/stat.err"
    rc=$?
    check "stat $path exits 0 ($(cat "$SANDBOX/stat.err"))" test "$rc" -eq 0
    jq_check "stat $path starts with the shared keys, in order" \
        "[keys_unsorted[]] | .[0:17] == $shared" "$SANDBOX/stat.json"
    jq_check "stat $path has every shared key typed" "$typed" "$SANDBOX/stat.json"
    jq_check "stat $path reports the path it was asked" ".path == \"$path\"" "$SANDBOX/stat.json"
done

fs.btrfs "$img" stat / >"$SANDBOX/root.json" 2>/dev/null
jq_check "stat / is the root directory, inode 256 in tree 5" '.type == "dir" and .inode == 256 and .btrfs.subvolume == 5' "$SANDBOX/root.json"
fs.btrfs "$img" stat /d >"$SANDBOX/d.json" 2>/dev/null
jq_check "stat /d is an empty directory: one link" '.type == "dir" and .nlink == 1' "$SANDBOX/d.json"
fs.btrfs "$img" stat /f >"$SANDBOX/f.json" 2>/dev/null
jq_check "stat /f is an empty file with one link" \
    '.type == "file" and .size == 0 and .nlink == 1 and .allocated_bytes == 0' "$SANDBOX/f.json"
fs.btrfs "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
check "stat /f and ls / agree on the inode and the mode" \
    test "$(jq -c '[.inode, .mode]' "$SANDBOX/f.json")" = "$(jq -c '.[] | select(.name == "f") | [.inode, .mode]' "$SANDBOX/ls.json")"

fs.btrfs "$img" stat --text /f >"$SANDBOX/f.txt" 2>/dev/null
check "stat --text /f exits 0" test $? -eq 0
check "stat --text /f prints key: value lines" grep -qx 'size: 0' "$SANDBOX/f.txt"
check "stat --text /f names the type" grep -qx 'type: file' "$SANDBOX/f.txt"

fs.btrfs "$img" stat /link >"$SANDBOX/link.json" 2>/dev/null
jq_check "stat reports the symlink, not its target" \
    '.type == "symlink" and .target == "f" and .size == 1' "$SANDBOX/link.json"
fs.btrfs "$img" stat "$raw" >"$SANDBOX/raw.json" 2>/dev/null
check "stat resolves a non-UTF-8 path" test $? -eq 0
jq_check "stat keeps the exact path bytes" '.path_hex == "2f7261772dff" and .type == "file"' "$SANDBOX/raw.json"

dd if=/dev/zero of="$SANDBOX/offset.img" bs=1048576 count=1 2>/dev/null
cat "$img" >>"$SANDBOX/offset.img"
fs.btrfs --offset 1048576 "$SANDBOX/offset.img" stat /f >"$SANDBOX/offset.json" 2>/dev/null
check "stat --offset resolves within the filesystem" test $? -eq 0
check "stat --offset reports the same metadata" cmp -s "$SANDBOX/f.json" "$SANDBOX/offset.json"

# Refusals: a structured error, nothing on stdout.
fs.btrfs "$img" stat /missing >"$SANDBOX/m.out" 2>"$SANDBOX/m.err"
check "stat of a missing path exits 1" test $? -eq 1
check "stat of a missing path prints nothing on stdout" test ! -s "$SANDBOX/m.out"
jq_check "stat of a missing path is the not_found kind" '.code == 1 and .kind == "not_found"' "$SANDBOX/m.err"
fs.btrfs "$img" stat >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "stat with no path is a usage error" test $? -eq 2
check "stat with no path prints nothing on stdout" test ! -s "$SANDBOX/u.out"

check "stat left the image as it was" cmp -s "$img" "$SANDBOX/before.img"

finish
