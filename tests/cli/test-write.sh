# fs.btrfs write, as installed, on a copy of the kernel-made CLI volume:
# an existing NODATACOW file overwritten in place at the same length
# round-trips byte for byte, and every write the library cannot make yet
# is refused with its reason and status 3 (or 1 for a path that is no
# file at all), leaving the image untouched. Then the names: mkdir, create,
# ln, rm and rmdir, and mkdir --parents making a missing chain (and nothing,
# without writing, for a chain already there).
#
# What btrfs-progs and the kernel make of the written volume is checked in
# tests/cli_write_kernel.rs, in the harness VM.
source "$(dirname "$0")/lib.sh"

src="$REPO/test-disks/cli/btrfs-cli.img"
manifest="$REPO/test-disks/cli/btrfs-cli.manifest"
if [ ! -s "$src" ] || [ ! -s "$manifest" ]; then
    fail "test-disks/cli/ is missing: the fixtures are built by \`chore fixtures\` (in the harness VM). Nothing skips."
    finish
fi

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi
}

# sparse_copy FROM TO
sparse_copy() {
    dd if="$1" of="$2" bs=1048576 conv=sparse status=none 2>/dev/null ||
        dd if="$1" of="$2" bs=1048576 conv=sparse 2>/dev/null
}

img="$SANDBOX/write.img"
sparse_copy "$src" "$img"
size="$(awk -F'\t' '$2 == "/nocow/data.bin" { print $3 }' "$manifest")"
check "the manifest gives /nocow/data.bin a size ($size)" test -n "$size"

# The write this library can make.
head -c "$size" /dev/urandom >"$SANDBOX/new.bin"
status=0
fs.btrfs "$img" write /nocow/data.bin <"$SANDBOX/new.bin" >"$SANDBOX/w.json" 2>"$SANDBOX/w.err" || status=$?
check "write /nocow/data.bin exits 0, not $status ($(cat "$SANDBOX/w.err"))" test "$status" -eq 0
jq_check "write reports the path and $size bytes, not created" \
    ".path == \"/nocow/data.bin\" and .bytes == $size and .created == false" "$SANDBOX/w.json"
check "read /nocow/data.bin is what was written" cmp -s "$SANDBOX/new.bin" <(fs.btrfs "$img" read /nocow/data.bin)
fs.btrfs "$img" ls /nocow >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the file keeps its size" ".[0].name == \"data.bin\" and .[0].size == $size" "$SANDBOX/ls.json"
want="$(awk -F'\t' '$2 == "/hello.txt" { print $4 }' "$manifest")"
check "the file beside it is untouched" test "$(fs.btrfs "$img" read /hello.txt | sha256_of)" = "$want"
check "the volume is not dirty after the write" test "$(fs.btrfs "$img" get dirty --text)" = false
fs.btrfs --text "$img" write /nocow/data.bin <"$SANDBOX/new.bin" >"$SANDBOX/t.out" 2>/dev/null
check "write --text says what it did" grep -q '^overwrote /nocow/data.bin' "$SANDBOX/t.out"

# Everything else is refused, and the image is left as it was.
sparse_copy "$img" "$SANDBOX/before.img"

# refused CODE PATTERN PATH STDIN-FILE
refused() {
    local code="$1" pattern="$2" path="$3" input="$4"
    local status=0
    fs.btrfs "$img" write "$path" <"$input" >"$SANDBOX/r.out" 2>"$SANDBOX/r.err" || status=$?
    check "write $path ($(basename "$input")) exits $code, not $status" test "$status" -eq "$code"
    check "write $path prints nothing on stdout" test ! -s "$SANDBOX/r.out"
    jq_check "write $path says why ($pattern)" ".code == $code and (.error | test(\"$pattern\"))" "$SANDBOX/r.err"
}
head -c "$((size + 4096))" /dev/urandom >"$SANDBOX/longer.bin"
head -c 10 /dev/urandom >"$SANDBOX/shorter.bin"
hello_size="$(awk -F'\t' '$2 == "/hello.txt" { print $3 }' "$manifest")"
random_size="$(awk -F'\t' '$2 == "/dir/random.bin" { print $3 }' "$manifest")"
head -c "$hello_size" /dev/urandom >"$SANDBOX/hello-sized.bin"
head -c "$random_size" /dev/urandom >"$SANDBOX/random-sized.bin"

refused 3 "grow the file" /nocow/data.bin "$SANDBOX/longer.bin"
refused 3 "keeps a file.s length, and truncate makes it shorter" /nocow/data.bin "$SANDBOX/shorter.bin"
refused 3 "no such file; .*create makes an empty one" /new.txt "$SANDBOX/shorter.bin"
refused 3 "no such file; .*create makes an empty one" /dir/new.txt "$SANDBOX/shorter.bin"
refused 3 "subvolume or snapshot" /vol/inside.txt "$SANDBOX/shorter.bin"
refused 1 "is a directory" /dir "$SANDBOX/shorter.bin"
refused 1 "not a regular file" /link "$SANDBOX/shorter.bin"
check "the refusals left the image as it was" cmp -s "$img" "$SANDBOX/before.img"

# A checksummed file is overwritten copy-on-write, its digests replaced
# (#261); tests/cow_write_oracle.rs has btrfs check and the kernel judge it.
status=0
fs.btrfs "$img" write /dir/random.bin <"$SANDBOX/random-sized.bin" >/dev/null 2>"$SANDBOX/c.err" || status=$?
check "write /dir/random.bin, a checksummed file, exits 0, not $status ($(cat "$SANDBOX/c.err"))" test "$status" -eq 0
check "read /dir/random.bin is what was written" \
    cmp -s "$SANDBOX/random-sized.bin" <(fs.btrfs "$img" read /dir/random.bin)

# An inline file is overwritten copy-on-write as well, and stored inline
# again (#438); tests/cow_write_oracle.rs has btrfs check and the kernel
# judge inline writes.
status=0
fs.btrfs "$img" write /hello.txt <"$SANDBOX/hello-sized.bin" >/dev/null 2>"$SANDBOX/i.err" || status=$?
check "write /hello.txt, an inline file, exits 0, not $status ($(cat "$SANDBOX/i.err"))" test "$status" -eq 0
check "read /hello.txt is what was written" \
    cmp -s "$SANDBOX/hello-sized.bin" <(fs.btrfs "$img" read /hello.txt)

# Names (#262): each verb commits one transaction; tests/namespace_kernel.rs
# has btrfs check and the kernel judge the same changes through the C ABI.
names="$SANDBOX/names.img"
sparse_copy "$src" "$names"
name_ok() {
    local status=0
    fs.btrfs "$names" "$@" >"$SANDBOX/n.json" 2>"$SANDBOX/n.err" || status=$?
    check "$* exits 0, not $status ($(cat "$SANDBOX/n.err"))" test "$status" -eq 0
}
name_ok mkdir /newdir
name_ok create --mode 600 /newdir/empty.txt
name_ok ln -s empty.txt /newdir/link
name_ok ln /hello.txt /newdir/hello-again
fs.btrfs "$names" ls /newdir >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the new directory lists its three names" \
    '[.[].name] | sort == ["empty.txt", "hello-again", "link"]' "$SANDBOX/ls.json"
check "the hard link reads as the file it names" \
    cmp -s <(fs.btrfs "$names" read /hello.txt) <(fs.btrfs "$names" read /newdir/hello-again)
name_ok rm /newdir/hello-again
name_ok rm /newdir/link
name_ok rm /newdir/empty.txt
name_ok rmdir /newdir
fs.btrfs "$names" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the directory is gone again" 'all(.[]; .name != "newdir")' "$SANDBOX/ls.json"
status=0
fs.btrfs "$names" rmdir /dir >/dev/null 2>"$SANDBOX/n.err" || status=$?
check "rmdir of a directory with entries exits 3, not $status" test "$status" -eq 3
jq_check "rmdir says the directory is not empty" '.code == 3 and (.error | test("not empty"))' "$SANDBOX/n.err"
# The last name of a file holding data: it goes, with its data (#262).
name_ok rm /dir/random.bin
fs.btrfs "$names" ls /dir >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the file holding data is gone" 'all(.[]; .name != "random.bin")' "$SANDBOX/ls.json"

# mkdir --parents (#414) makes every missing directory on the way to PATH,
# mode 0755 (--mode is PATH's own), and reports only those; a directory
# already there is passed through, a subvolume's top included.
# tests/cli_mkdir_kernel.rs has btrfs check and the kernel judge the chains,
# and the kernel's own mkdir -p the refusals.
parents="$SANDBOX/parents.img"
sparse_copy "$src" "$parents"
sparse_copy "$parents" "$SANDBOX/before-p.img"
status=0
fs.btrfs "$parents" mkdir /m1/m2 >"$SANDBOX/p.out" 2>"$SANDBOX/p.err" || status=$?
check "plain mkdir under a missing parent exits 1, not $status" test "$status" -eq 1
jq_check "plain mkdir makes no parent" '.code == 1 and (.error | test("no such file"))' "$SANDBOX/p.err"
check "plain mkdir under a missing parent left the image as it was" cmp -s "$parents" "$SANDBOX/before-p.img"
p_ok() {
    local status=0
    fs.btrfs "$parents" mkdir "$@" >"$SANDBOX/p.json" 2>"$SANDBOX/p.err" || status=$?
    check "mkdir $* exits 0, not $status ($(cat "$SANDBOX/p.err"))" test "$status" -eq 0
}
p_ok --parents /p/q/r
jq_check "mkdir --parents reports the path and each directory it made" \
    '.path == "/p/q/r" and .created == ["/p", "/p/q", "/p/q/r"] and (keys | length) == 2' \
    "$SANDBOX/p.json"
for name in /p /p/q /p/q/r; do
    fs.btrfs "$parents" stat "$name" >"$SANDBOX/stat.json" 2>/dev/null
    jq_check "$name is a directory, mode 0755, with the one link a Btrfs directory has" \
        '.type == "dir" and .mode == "0755" and .nlink == 1' "$SANDBOX/stat.json"
done
p_ok -p /dir/sub/s/t
jq_check "mkdir -p under an existing chain makes only what is missing" \
    '.path == "/dir/sub/s/t" and .created == ["/dir/sub/s", "/dir/sub/s/t"]' "$SANDBOX/p.json"
p_ok -p /u/../v
jq_check "mkdir -p makes a directory that .. then leaves" '.created == ["/u", "/u/../v"]' "$SANDBOX/p.json"
p_ok -p /vol/../outside
jq_check "mkdir -p passes into a subvolume and back out of it" \
    '.created == ["/vol/../outside"]' "$SANDBOX/p.json"
for name in /u /v /outside; do
    check "$name was made" test "$(fs.btrfs "$parents" stat "$name" 2>/dev/null | jq -r .type)" = dir
done
p_ok -p --mode 700 /m/n
check "--mode is not given to a directory on the way: /m is 0755" \
    test "$(fs.btrfs "$parents" stat /m 2>/dev/null | jq -r .mode)" = 0755
check "--mode is given to PATH: /m/n is 0700" \
    test "$(fs.btrfs "$parents" stat /m/n 2>/dev/null | jq -r .mode)" = 0700
fs.btrfs --text "$parents" mkdir -p /w/x >"$SANDBOX/p.txt" 2>/dev/null
check "mkdir -p --text names each directory made" \
    test "$(cat "$SANDBOX/p.txt")" = "$(printf 'created /w\ncreated /w/x')"
check "the volume is not dirty after mkdir -p" test "$(fs.btrfs "$parents" get dirty --text)" = false

# A chain that is already there is success, and nothing is written.
sparse_copy "$parents" "$SANDBOX/chain.img"
for p in /p/q/r /p/q/r/ /p/./q/.. / /vol /snap /vol/../dir/sub; do
    status=0
    fs.btrfs "$parents" mkdir -p "$p" >"$SANDBOX/p.json" 2>"$SANDBOX/p.err" || status=$?
    check "mkdir -p $p, already there, exits 0, not $status ($(cat "$SANDBOX/p.err"))" test "$status" -eq 0
    jq_check "mkdir -p $p reports nothing made" ".path == \"$p\" and .created == []" "$SANDBOX/p.json"
done
fs.btrfs --text "$parents" mkdir -p /p/q >"$SANDBOX/p.txt" 2>/dev/null
check "mkdir -p --text says when nothing was made" \
    test "$(cat "$SANDBOX/p.txt")" = "/p/q exists; nothing created"
check "a chain already there was not written to" cmp -s "$parents" "$SANDBOX/chain.img"

# Refusals: anything but a directory on the way, a name too long or an
# empty path exits 1, and a directory inside a subvolume other than the
# top level, which only the kernel writes, exits 3. Each is decided before
# anything is made, so no partial chain is left and the image is
# byte-identical. A symlink is never followed, even one naming a directory.
fs.btrfs "$parents" ln -s dir /dl >/dev/null 2>&1
check "ln -s dir /dl made a symlink to a directory" \
    test "$(fs.btrfs "$parents" stat /dl 2>/dev/null | jq -r .type)" = symlink
sparse_copy "$parents" "$SANDBOX/before-p.img"
long="$(printf 'n%.0s' $(seq 256))"
# p_refused CODE WHY PATH
p_refused() {
    local code="$1" why="$2" p="$3" status=0
    fs.btrfs "$parents" mkdir -p "$p" >"$SANDBOX/x.out" 2>"$SANDBOX/x.err" || status=$?
    check "mkdir -p ${p:0:40} exits $code, not $status" test "$status" -eq "$code"
    check "mkdir -p ${p:0:40} prints nothing on stdout" test ! -s "$SANDBOX/x.out"
    jq_check "mkdir -p ${p:0:40} is refused: $why" ".code == $code and (.error | test(\"$why\"))" "$SANDBOX/x.err"
    check "mkdir -p ${p:0:40} left the image byte-identical" cmp -s "$parents" "$SANDBOX/before-p.img"
}
p_refused 1 "already exists" /hello.txt
p_refused 1 "already exists" /hello.txt/
p_refused 1 "not a directory" /hello.txt/x
p_refused 1 "not a directory" /hello.txt/x/y
p_refused 1 "not a directory" /new/../hello.txt/x
p_refused 1 "not a directory" /dir/sub/deep.txt/x
p_refused 1 "not a directory" /vol/inside.txt/x
p_refused 1 "name too long" "/n1/n2/$long"
p_refused 1 "not a directory" "/hello.txt/$long"
p_refused 1 "already exists" /link
p_refused 1 "not a directory" /link/x
p_refused 1 "already exists" /dl
p_refused 1 "not a directory" /dl/x
p_refused 1 "no such file or directory" ""
p_refused 3 "subvolume" /vol/new
p_refused 3 "subvolume" /vol/a/b
p_refused 3 "subvolume" /snap/x
p_refused 3 "subvolume" /new/../vol/x

# --offset is where the filesystem starts: a MiB of zeros before it is
# never touched.
off="$SANDBOX/offset.img"
dd if="$src" of="$off" bs=1048576 seek=1 conv=sparse status=none 2>/dev/null ||
    dd if="$src" of="$off" bs=1048576 seek=1 conv=sparse 2>/dev/null
status=0
fs.btrfs --offset 1048576 "$off" mkdir -p /o1/o2 >/dev/null 2>"$SANDBOX/o.err" || status=$?
check "mkdir -p with --offset exits 0, not $status ($(cat "$SANDBOX/o.err"))" test "$status" -eq 0
check "mkdir -p with --offset made the chain" \
    test "$(fs.btrfs --offset 1048576 "$off" stat /o1/o2 2>/dev/null | jq -r .type)" = dir
check "the MiB before --offset is still zeros" \
    cmp -s <(head -c 1048576 "$off") <(head -c 1048576 /dev/zero)

finish
