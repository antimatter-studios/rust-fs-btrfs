//! Ranges cloned through the C ABI, judged by what is not this crate
//! (#269).
//!
//! A kernel-made volume holds a file of three extents, a file whose last
//! sector is partial, a file to clone into itself, empty destinations, a
//! file whose data is not checksummed, an extent the kernel reflinked six
//! times and one it reflinked forty-one times. Through the ABI the first
//! is cloned whole into one file and in part, across an extent boundary,
//! into another; the second to its end into six files, so one extent
//! gains six inline references from this crate in one go; and the third
//! into itself past its end. Each call is one committed transaction.
//!
//! Then the independent oracles judge it:
//!
//! - `btrfs check` inspects the whole volume, which reports an extent
//!   whose reference count or back references disagree with the file
//!   items naming it, and `--check-data-csum` compares every data sector
//!   with its digest;
//! - `dump-tree` must show the hash that orders data references to be the
//!   kernel's: every reference the kernel filed as an item of its own is
//!   keyed by [`data_ref_hash`] of its fields, and the kernel keeps the
//!   inline ones in descending order of it, as this crate does;
//! - the kernel mounts the volume and reports each file's size and
//!   SHA-256, and `filefrag` must show each clone on the same physical
//!   blocks as its source, flagged shared;
//! - the kernel makes the same clones itself, through `FICLONERANGE`,
//!   into twin files, and `dump-tree` must show the same extent items in
//!   both;
//! - and the kernel writes to, truncates and deletes the clones and their
//!   sources, which finds and drops each reference this crate added, and
//!   `btrfs check` looks again.

use fs_btrfs::capi::*;
use fs_btrfs::extent_write::data_ref_hash;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path,
};
use std::collections::BTreeMap;
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

const ENOENT: i32 = 2;
const EISDIR: i32 = 21;
const EROFS: i32 = 30;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

/// The files `/odd` is cloned into, beside `/odd2`.
const RS: [&str; 5] = ["r1", "r2", "r3", "r4", "r5"];

/// The kernel's own clones, through `FICLONERANGE`, of what the test
/// clones through the ABI, into twin files; then each twin's inode.
/// `0x4020940d` is `_IOW(0x94, 13, struct file_clone_range)`, whose
/// fields are the source's descriptor, its offset, the length and the
/// destination's offset.
const KERNEL_CLONES: &str = r#"cd "$MNT"
python3 - <<'PY'
import fcntl, os, struct
FICLONERANGE = 0x4020940d
for src, src_offset, dst, dst_offset, length in [
    ('multi', 0, 'kdst', 0, 0),
    ('multi', 61440, 'kpart', 4096, 8192),
    ('odd', 0, 'kodd2', 0, 0),
]:
    s = os.open(src, os.O_RDONLY)
    d = os.open(dst, os.O_WRONLY | os.O_CREAT, 0o644)
    fcntl.ioctl(d, FICLONERANGE, struct.pack('=qQQQ', s, src_offset, length, dst_offset))
    os.close(s)
    os.close(d)
PY
sync
for f in kdst kpart kodd2; do printf '%s %s\n' "$f" "$(stat -c %i "$f")"; done
"#;

/// A scratch volume, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "16384", "-O", "no-holes"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(
            &image.to_string_lossy(),
            name,
            "cd \"$MNT\"\n\
             head -c 65536 /dev/urandom > multi\n\
             sync\n\
             head -c 65536 /dev/urandom >> multi\n\
             sync\n\
             head -c 65536 /dev/urandom >> multi\n\
             sync\n\
             head -c 10000 /dev/urandom > odd\n\
             head -c 65536 /dev/urandom > self\n\
             head -c 8192 /dev/urandom > full\n\
             touch dst part odd2 r1 r2 r3 r4 r5 blank\n\
             touch nosum\n\
             chattr +C nosum\n\
             head -c 8192 /dev/urandom >> nosum\n\
             head -c 4096 /dev/urandom > k\n\
             for i in 1 2 3 4 5; do cp --reflink=always k k.$i; done\n\
             head -c 4096 /dev/urandom > many\n\
             for i in $(seq 1 40); do cp --reflink=always many many.$i; done\n\
             mkdir dir\n\
             sync",
        );
        Scratch(dir)
    }

    fn image(&self) -> String {
        self.0.join("fs.img").to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn clone(fs: *mut fs_btrfs_fs, src: &str, src_offset: u64, dst: &str, at: u64, len: u64) -> i32 {
    unsafe { fs_btrfs_clone_range(fs, c(src).as_ptr(), src_offset, c(dst).as_ptr(), at, len) }
}

#[track_caller]
fn cloned(fs: *mut fs_btrfs_fs, src: &str, src_offset: u64, dst: &str, at: u64, len: u64) {
    assert_eq!(
        clone(fs, src, src_offset, dst, at, len),
        0,
        "clone {src} {src_offset} -> {dst} {at} ({len}): {}",
        last_error()
    );
}

#[track_caller]
fn refused(
    fs: *mut fs_btrfs_fs,
    src: &str,
    src_offset: u64,
    dst: &str,
    at: u64,
    len: u64,
    want: i32,
) {
    assert_eq!(
        clone(fs, src, src_offset, dst, at, len),
        -1,
        "clone {src} {src_offset} -> {dst} {at} ({len}) succeeded and should not have"
    );
    assert_eq!(
        fs_btrfs_last_errno(),
        want,
        "clone {src} {src_offset} -> {dst} {at} ({len}): wrong errno for {:?}",
        last_error()
    );
}

fn read(fs: *mut fs_btrfs_fs, path: &str, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            c(path).as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            len as u64,
        )
    };
    assert!(n >= 0, "read {path}: {}", last_error());
    buf.truncate(n as usize);
    buf
}

/// One `EXTENT_DATA_REF`, inline or an item of its own, as `dump-tree`
/// prints it: `extent data backref root R objectid O offset F count C`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DataRef {
    root: u64,
    objectid: u64,
    offset: u64,
    count: u64,
}

impl DataRef {
    fn hash(&self) -> u64 {
        data_ref_hash(self.root, self.objectid, self.offset)
    }
}

/// A data extent's record: its total and its inline data references, in
/// the order the item holds them.
#[derive(Debug, Default)]
struct Record {
    refs: u64,
    inline: Vec<DataRef>,
}

/// The extent tree as `dump-tree -t extent` prints it: every data
/// extent's record by address, and every data reference filed as an item
/// of its own, with the hash its key carries.
struct ExtentTree {
    records: BTreeMap<u64, Record>,
    keyed: Vec<(u64, DataRef)>,
}

/// The `(objectid TYPE offset)` of an `item N key (...)` line.
fn item_key(line: &str) -> Option<(u64, String, u64)> {
    let rest = line.trim_start().strip_prefix("item ")?;
    let (_, key) = rest.split_once("key (")?;
    let (key, _) = key.split_once(')')?;
    let mut words = key.split_whitespace();
    let objectid = words.next()?.parse().ok()?;
    let kind = words.next()?.to_string();
    let offset = words.next()?.parse().ok()?;
    Some((objectid, kind, offset))
}

fn data_ref(line: &str) -> Option<DataRef> {
    let (_, rest) = line.split_once("extent data backref root ")?;
    let words: Vec<&str> = rest.split_whitespace().collect();
    let root = match words[0] {
        "FS_TREE" => 5,
        n => n
            .parse()
            .unwrap_or_else(|_| panic!("a root dump-tree names as {n:?}: {line:?}")),
    };
    let field = |name: &str| -> u64 {
        let at = words
            .iter()
            .position(|w| *w == name)
            .unwrap_or_else(|| panic!("no {name} in {line:?}"));
        words[at + 1].parse().unwrap()
    };
    Some(DataRef {
        root,
        objectid: field("objectid"),
        offset: field("offset"),
        count: field("count"),
    })
}

fn extent_tree(image: &str) -> ExtentTree {
    let dump = dump_tree(Path::new(image), "extent");
    let mut tree = ExtentTree {
        records: BTreeMap::new(),
        keyed: Vec::new(),
    };
    let mut current: Option<(u64, String, u64)> = None;
    for line in dump.lines() {
        if let Some(key) = item_key(line) {
            if key.1 == "EXTENT_ITEM" {
                tree.records.insert(key.0, Record::default());
            }
            current = Some(key);
            continue;
        }
        let Some((bytenr, kind, offset)) = &current else {
            continue;
        };
        if kind == "EXTENT_ITEM" && line.trim_start().starts_with("refs ") {
            let refs = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            tree.records.get_mut(bytenr).unwrap().refs = refs;
        } else if let Some(r) = data_ref(line) {
            match kind.as_str() {
                "EXTENT_ITEM" => tree.records.get_mut(bytenr).unwrap().inline.push(r),
                "EXTENT_DATA_REF" => tree.keyed.push((*offset, r)),
                _ => {}
            }
        }
    }
    tree
}

impl ExtentTree {
    /// The record whose inline references include one from `ino`.
    fn record_naming(&self, ino: u64) -> &Record {
        let found: Vec<&Record> = self
            .records
            .values()
            .filter(|r| r.inline.iter().any(|d| d.root == 5 && d.objectid == ino))
            .collect();
        assert_eq!(
            found.len(),
            1,
            "dump-tree shows {} extents inline-referenced by inode {ino}",
            found.len()
        );
        found[0]
    }
}

#[track_caller]
fn assert_descending_hashes(record: &Record, what: &str) {
    let hashes: Vec<u64> = record.inline.iter().map(DataRef::hash).collect();
    let mut sorted = hashes.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        hashes, sorted,
        "[{what}] the inline data references are not in descending hash order: {:?}",
        record.inline
    );
}

/// Every regular `EXTENT_DATA` item of inode `ino` in the top-level
/// tree: its key offset, disk address, disk length, offset into the
/// extent and length in the file, which is what a clone copies. The
/// generation is left out; it is the transaction's, not the extent's.
fn extent_items(fs_tree: &str, ino: u64) -> Vec<[u64; 5]> {
    let lines: Vec<&str> = fs_tree.lines().collect();
    let mut items = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some((objectid, kind, key_offset)) = item_key(line) else {
            continue;
        };
        if objectid != ino || kind != "EXTENT_DATA" {
            continue;
        }
        let body: Vec<&str> = lines[at + 1..]
            .iter()
            .take_while(|l| item_key(l).is_none())
            .copied()
            .collect();
        let disk = body
            .iter()
            .find(|l| l.contains("extent data disk byte"))
            .unwrap_or_else(|| panic!("inode {ino}'s item at {key_offset} has no disk byte"));
        let within = body
            .iter()
            .find(|l| l.contains("extent data offset"))
            .unwrap_or_else(|| panic!("inode {ino}'s item at {key_offset} has no offset"));
        items.push([
            key_offset,
            after(disk, "byte"),
            after(disk, "nr"),
            after(within, "offset"),
            after(within, "nr"),
        ]);
    }
    items
}

fn after(line: &str, word: &str) -> u64 {
    let mut it = line.split_whitespace();
    while let Some(w) = it.next() {
        if w == word {
            return it.next().unwrap().parse().unwrap();
        }
    }
    panic!("no {word} in {line:?}")
}

/// Each file's blocks, logical to physical with whether `filefrag -v`
/// calls the extent shared, from a script that prints `== NAME` before
/// each file's report.
fn block_maps(report: &str) -> BTreeMap<String, BTreeMap<u64, (u64, bool)>> {
    let mut maps: BTreeMap<String, BTreeMap<u64, (u64, bool)>> = BTreeMap::new();
    let mut name = String::new();
    for line in report.lines() {
        if let Some(n) = line.strip_prefix("== ") {
            name = n.to_string();
            maps.entry(name.clone()).or_default();
            continue;
        }
        let words: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == ':' || c == '.')
            .filter(|w| !w.is_empty())
            .collect();
        let is_extent = line
            .trim_start()
            .split(':')
            .next()
            .is_some_and(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit()));
        if !is_extent {
            continue;
        }
        let num = |i: usize| -> u64 { words[i].parse().unwrap_or_else(|_| panic!("{line:?}")) };
        let (logical, physical, len) = (num(1), num(3), num(5));
        let shared = line.contains("shared");
        let map = maps.get_mut(&name).expect("a file's report");
        for i in 0..len {
            map.insert(logical + i, (physical + i, shared));
        }
    }
    maps
}

#[test]
fn ranges_cloned_through_the_abi_share_their_extents_and_the_kernel_agrees() {
    let scratch = Scratch::new("clone-kernel");
    let image = scratch.image();

    // THE ORACLE FOR THE HASH, before anything here writes: the kernel
    // keys each data reference it files as an item of its own by the
    // hash of its fields, and keeps the inline ones in descending order
    // of it. Neither check may pass for want of a case.
    let before = extent_tree(&image);
    assert!(
        !before.keyed.is_empty(),
        "forty-one reflinks of /many left no data reference filed as an item of its own"
    );
    for (hash, r) in &before.keyed {
        assert_eq!(
            *hash,
            r.hash(),
            "the kernel keys the data reference {r:?} by {hash}, and data_ref_hash gives {}",
            r.hash()
        );
    }

    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    let multi = read(fs, "/multi", 1 << 20);
    assert_eq!(multi.len(), 196_608, "/multi is not the size made");
    let odd = read(fs, "/odd", 1 << 20);
    assert_eq!(odd.len(), 10_000, "/odd is not the size made");
    let selfish = read(fs, "/self", 1 << 20);
    assert_eq!(selfish.len(), 65_536, "/self is not the size made");

    // A whole file of three extents, as `cp --reflink` does it.
    cloned(fs, "/multi", 0, "/dst", 0, 0);
    // Two sectors across the first extent's end, at an offset into the
    // destination: each item windowed into its extent.
    cloned(fs, "/multi", 61_440, "/part", 4096, 8192);
    // To the end of a file whose last sector is partial, six times: one
    // extent gains six inline references here, each where the kernel
    // looks for it.
    cloned(fs, "/odd", 0, "/odd2", 0, 0);
    for r in RS {
        cloned(fs, "/odd", 0, &format!("/{r}"), 0, 10_000);
    }
    // A file into itself, past its end: one inode, two references.
    cloned(fs, "/self", 0, "/self", 131_072, 65_536);

    let mut part = vec![0u8; 4096];
    part.extend_from_slice(&multi[61_440..69_632]);
    let mut self_after = selfish.clone();
    self_after.resize(131_072, 0);
    self_after.extend_from_slice(&selfish);
    assert!(read(fs, "/dst", 1 << 20) == multi, "/dst is not /multi");
    assert!(
        read(fs, "/part", 1 << 20) == part,
        "/part is not its window"
    );
    assert!(read(fs, "/odd2", 1 << 20) == odd, "/odd2 is not /odd");
    assert!(
        read(fs, "/self", 1 << 20) == self_after,
        "/self is not twice itself"
    );

    // Each refusal names its errno and writes nothing.
    let unchanged = std::fs::read(&image).unwrap();
    // Offsets that are not whole sectors.
    refused(fs, "/multi", 100, "/blank", 0, 4096, ENOTSUP);
    refused(fs, "/multi", 0, "/blank", 100, 4096, ENOTSUP);
    // A length that is neither whole sectors nor to the end.
    refused(fs, "/multi", 0, "/blank", 0, 5000, ENOTSUP);
    // Past the source's end.
    refused(fs, "/odd", 0, "/blank", 0, 20_000, ENOTSUP);
    // Over data the destination already holds.
    refused(fs, "/multi", 0, "/full", 0, 4096, ENOTSUP);
    // A file into itself, over the range it reads.
    refused(fs, "/self", 0, "/self", 4096, 65_536, ENOTSUP);
    // A checksummed file into one that is not, and back.
    refused(fs, "/nosum", 0, "/blank", 0, 0, ENOTSUP);
    refused(fs, "/multi", 0, "/nosum", 65_536, 4096, ENOTSUP);
    // An extent whose references the kernel filed as items of their own.
    refused(fs, "/many", 0, "/blank", 0, 0, ENOTSUP);
    refused(fs, "/dir", 0, "/blank", 0, 0, EISDIR);
    refused(fs, "/multi", 0, "/dir", 0, 0, EISDIR);
    refused(fs, "/missing", 0, "/blank", 0, 0, ENOENT);
    refused(fs, "/multi", 0, "/missing", 0, 0, ENOENT);
    assert!(
        std::fs::read(&image).unwrap() == unchanged,
        "a refused clone wrote to the image"
    );
    unsafe { fs_btrfs_umount(fs) };

    let ro = unsafe { fs_btrfs_mount(c(&image).as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    refused(ro, "/multi", 0, "/blank", 0, 0, EROFS);
    unsafe { fs_btrfs_umount(ro) };
    assert!(
        std::fs::read(&image).unwrap() == unchanged,
        "a read-only handle wrote to the image"
    );

    assert_btrfs_check_clean(
        Path::new(&image),
        "after ranges were cloned through the ABI",
    );
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    // What the kernel reads, which inode each file is, and where each
    // file's blocks are.
    let names = [
        "multi", "dst", "part", "odd", "odd2", "r1", "r2", "r3", "r4", "r5", "self", "k",
    ];
    let seen = guest_kernel_read_ok(
        &image,
        "after the ABI's clones",
        &format!(
            "cd \"$MNT\"\n\
             for f in {all}; do \
               printf '%s %s %s %s\\n' \"$f\" \"$(stat -c %s \"$f\")\" \
                 \"$(sha256sum \"$f\" | cut -d' ' -f1)\" \"$(stat -c %i \"$f\")\"; done\n\
             for f in {all}; do echo \"== $f\"; filefrag -v \"$f\"; done",
            all = names.join(" ")
        ),
    );
    let mut ino = BTreeMap::new();
    let mut sums = String::new();
    for line in seen.lines().take_while(|l| !l.starts_with("== ")) {
        let words: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(words.len(), 4, "the kernel's report: {line:?}");
        ino.insert(words[0].to_string(), words[3].parse::<u64>().unwrap());
        if words[0] != "k" {
            sums.push_str(&format!("{} {} {}\n", words[0], words[1], words[2]));
        }
    }
    let want: String = [
        ("multi", &multi),
        ("dst", &multi),
        ("part", &part),
        ("odd", &odd),
        ("odd2", &odd),
        ("r1", &odd),
        ("r2", &odd),
        ("r3", &odd),
        ("r4", &odd),
        ("r5", &odd),
        ("self", &self_after),
    ]
    .iter()
    .map(|(name, bytes)| format!("{name} {} {}\n", bytes.len(), sha256_hex(bytes)))
    .collect();
    assert_eq!(sums, want, "the kernel reads the cloned files differently");

    // `filefrag`: every clone on its source's physical blocks, shared.
    let maps = block_maps(&seen);
    let map = |name: &str| &maps[name];
    assert_eq!(
        map("multi").len(),
        48,
        "/multi's blocks: {:?}",
        map("multi")
    );
    assert_eq!(map("dst"), map("multi"), "/dst is not on /multi's blocks");
    assert_eq!(map("odd2"), map("odd"), "/odd2 is not on /odd's blocks");
    for r in RS {
        assert_eq!(map(r), map("odd"), "/{r} is not on /odd's blocks");
    }
    let windowed: BTreeMap<u64, (u64, bool)> =
        [(1, map("multi")[&15]), (2, map("multi")[&16])].into();
    assert_eq!(
        map("part"),
        &windowed,
        "/part is not on /multi's blocks 15 and 16"
    );
    for i in 0..16 {
        assert_eq!(
            map("self").get(&(32 + i)),
            map("self").get(&i),
            "/self's block {} is not on its block {i}",
            32 + i
        );
    }
    for name in ["multi", "dst", "part", "odd", "odd2", "self"] {
        assert!(
            map(name).values().all(|&(_, shared)| shared),
            "filefrag does not call all of /{name} shared: {:?}",
            map(name)
        );
    }

    // `dump-tree`: the extent /odd's clones share counts seven
    // references, the six added here inline beside the original, in the
    // order the kernel keeps its own six for /k.
    let after_clones = extent_tree(&image);
    let kernels = after_clones.record_naming(ino["k"]);
    assert_eq!(
        kernels.inline.len(),
        6,
        "the kernel's reflinks of /k: {kernels:?}"
    );
    assert_descending_hashes(kernels, "the kernel's reflinks of /k");
    let ours = after_clones.record_naming(ino["odd"]);
    assert_eq!(ours.refs, 7, "the extent /odd's clones share: {ours:?}");
    let mut named: Vec<u64> = ours.inline.iter().map(|d| d.objectid).collect();
    named.sort_unstable();
    let mut want: Vec<u64> = ["odd", "odd2", "r1", "r2", "r3", "r4", "r5"]
        .iter()
        .map(|n| ino[*n])
        .collect();
    want.sort_unstable();
    assert_eq!(
        named, want,
        "the inodes referencing /odd's extent: {ours:?}"
    );
    assert!(
        ours.inline
            .iter()
            .all(|d| d.root == 5 && d.offset == 0 && d.count == 1),
        "the references to /odd's extent: {ours:?}"
    );
    assert_descending_hashes(ours, "the references this crate added to /odd's extent");
    let selves = after_clones.record_naming(ino["self"]);
    assert_eq!(selves.refs, 2, "/self's extent: {selves:?}");
    let mut at: Vec<u64> = selves.inline.iter().map(|d| d.offset).collect();
    at.sort_unstable();
    assert_eq!(at, [0, 131_072], "/self's references: {selves:?}");

    // Every sector the kernel can reach, against its digest.
    guest_kernel_write_ok(&image, "scrub", "btrfs scrub start -B \"$MNT\" >/dev/null");

    // The kernel makes the same clones itself, into twins, and its items
    // must be the ones written here.
    let twins = guest_kernel_write_ok(&image, "the kernel's own clones", KERNEL_CLONES);
    for line in twins.lines() {
        let (name, n) = line.split_once(' ').unwrap();
        ino.insert(name.to_string(), n.parse().unwrap());
    }
    let fs_tree = dump_tree(Path::new(&image), "5");
    for (ours, kernels) in [("dst", "kdst"), ("part", "kpart"), ("odd2", "kodd2")] {
        let mine = extent_items(&fs_tree, ino[ours]);
        assert!(
            !mine.is_empty(),
            "dump-tree shows no extent items for /{ours}"
        );
        assert_eq!(
            mine,
            extent_items(&fs_tree, ino[kernels]),
            "/{ours}'s extent items are not the ones the kernel made for /{kernels}"
        );
    }
    let after_kernel = extent_tree(&image);
    let shared = after_kernel.record_naming(ino["odd"]);
    assert_eq!(
        shared.refs, 8,
        "/odd's extent after the kernel's clone: {shared:?}"
    );
    assert_descending_hashes(
        shared,
        "after the kernel added its own reference among these",
    );

    // The kernel writes into one clone, truncates another and deletes
    // sources and clones alike: each reference added here is one it has
    // to find to drop. The checker looks again.
    guest_kernel_write_ok(
        &image,
        "the kernel changes the clones",
        "cd \"$MNT\"\n\
         rm odd r1 r3 part\n\
         head -c 4096 /dev/urandom | dd of=dst bs=4096 seek=1 conv=notrunc status=none\n\
         truncate -s 4096 self\n\
         sync",
    );
    assert_btrfs_check_clean(Path::new(&image), "after the kernel changed the clones");
    let last = guest_kernel_read_ok(
        &image,
        "after the kernel's changes",
        "cd \"$MNT\"\n\
         for f in odd2 r2 r4 r5 kodd2 multi kdst self; do \
           printf '%s %s %s\\n' \"$f\" \"$(stat -c %s \"$f\")\" \
             \"$(sha256sum \"$f\" | cut -d' ' -f1)\"; done\n\
         printf 'dst %s %s %s\\n' \"$(stat -c %s dst)\" \
           \"$(head -c 4096 dst | sha256sum | cut -d' ' -f1)\" \
           \"$(tail -c +8193 dst | sha256sum | cut -d' ' -f1)\"",
    );
    let mut want: String = [
        ("odd2", &odd[..]),
        ("r2", &odd[..]),
        ("r4", &odd[..]),
        ("r5", &odd[..]),
        ("kodd2", &odd[..]),
        ("multi", &multi[..]),
        ("kdst", &multi[..]),
        ("self", &selfish[..4096]),
    ]
    .iter()
    .map(|(name, bytes)| format!("{name} {} {}\n", bytes.len(), sha256_hex(bytes)))
    .collect();
    want.push_str(&format!(
        "dst {} {} {}\n",
        multi.len(),
        sha256_hex(&multi[..4096]),
        sha256_hex(&multi[8192..])
    ));
    assert_eq!(
        last, want,
        "the kernel reads the clones differently after changing them"
    );
}
