//! Ranges preallocated and punched through the C ABI, judged by what is not
//! this crate (#713).
//!
//! The kernel makes a volume of twin files, each pair the same recipe of the
//! same bytes. Each case below changes one twin through the ABI and the other
//! through the kernel's own `fallocate` and `fallocate -p`. Then:
//!
//! - `btrfs check --check-data-csum` inspects the whole volume, and a kernel
//!   scrub reads it;
//! - the kernel must read the same bytes from both twins, and so must the ABI;
//! - `dump-tree` must show the same inode size, byte count and flags, the same
//!   extent items and the same data references for both;
//! - `filefrag -v` must flag the same bytes unwritten in both;
//! - each case must move the times the kernel moves, or leave them;
//! - and the kernel and the ABI then write into, punch and delete the files
//!   under a snapshot, which finds and drops each reference added here, and
//!   the checker looks again.

#![allow(unsafe_code)]

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};

const ENOENT: i32 = 2;
const EISDIR: i32 = 21;
const EROFS: i32 = 30;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

const K: u64 = 1024;
const KEEP: u32 = FS_BTRFS_FALLOC_KEEP_SIZE;
const PUNCH: u32 = FS_BTRFS_FALLOC_PUNCH_HOLE | FS_BTRFS_FALLOC_KEEP_SIZE;

/// One change: what it covers, the recipe both twins start from, and the
/// `fallocate(2)` mode, offset and length. Twins are named for their place in
/// [`CASES`]: `a0` for the ABI and `k0` for the kernel.
struct Case {
    what: &'static str,
    recipe: &'static str,
    mode: u32,
    offset: u64,
    len: u64,
}

const fn case(what: &'static str, recipe: &'static str, mode: u32, offset: u64, len: u64) -> Case {
    Case {
        what,
        recipe,
        mode,
        offset,
        len,
    }
}

const CASES: [Case; 26] = [
    // Holes of a range become preallocated items, and the size follows
    // unless asked not to.
    case("empty file", "empty", 0, 0, 1024 * K),
    case("empty file, size kept", "empty", KEEP, 0, 1024 * K),
    case("over a data item's end", "data64", 0, 32 * K, 64 * K),
    case("all data: nothing to allocate", "data64", 0, 4 * K, 8 * K),
    case("two holes around data", "sparse", 0, 0, 64 * K),
    case("starting past the end", "data8", 0, 64 * K, 8 * K),
    case("past the end, size kept", "data8", KEEP, 16 * K, 16 * K),
    case("offsets off sectors", "empty", 0, K, 5 * K),
    case("past a partial last sector", "odd", 0, 0, 64 * K),
    case(
        "clear of a partial last sector, size kept",
        "odd",
        KEEP,
        8 * K,
        8 * K,
    ),
    case(
        "inside space already preallocated",
        "prealloc",
        0,
        4 * K,
        8 * K,
    ),
    case("past space kept past the end", "kept", 0, 0, 128 * K),
    case("over a kept item, size kept", "kept", KEEP, 32 * K, 64 * K),
    // Items over a range are deleted or cut, and their extents released or
    // a reference dropped.
    case("through a data item", "data64", PUNCH, 16 * K, 16 * K),
    case("a whole extent", "data64", PUNCH, 0, 64 * K),
    case("across two extents", "two", PUNCH, 32 * K, 64 * K),
    case(
        "through a preallocated item",
        "prealloc",
        PUNCH,
        16 * K,
        16 * K,
    ),
    case("a whole reflinked extent", "reflink", PUNCH, 0, 64 * K),
    case(
        "through a reflinked extent",
        "reflink",
        PUNCH,
        16 * K,
        16 * K,
    ),
    case(
        "past the end, over a kept item",
        "kept",
        PUNCH,
        16 * K,
        128 * K,
    ),
    case("only a hole", "sparse", PUNCH, 8 * K, 24 * K),
    // Part of a sector is zeroed, as the kernel zeroes it.
    case("off sectors at both ends", "data64", PUNCH, K, 8 * K),
    case("inside one sector", "data64", PUNCH, 100, 200),
    case("to the end of a partial last sector", "odd", PUNCH, 0, 5000),
    case("off sectors, preallocated", "prealloc", PUNCH, K, 8 * K),
    case("past the end from data", "data64", PUNCH, 60 * K, 8 * K),
];

/// Make the shared sources, then every recipe for every twin. Twins are
/// written with `dd`, because `cp` may reflink.
fn make_script() -> String {
    let mut script = String::from(
        "cd \"$MNT\"\n\
         head -c 131072 /dev/urandom > source\n\
         head -c 65536 /dev/urandom > shared\n\
         sync\n\
         put() { dd if=source of=\"$1\" bs=4096 skip=\"$2\" seek=\"$3\" count=\"$4\" conv=notrunc status=none; sync; }\n\
         empty() { : > \"$1\"; }\n\
         data8() { put \"$1\" 0 0 2; }\n\
         data64() { put \"$1\" 0 0 16; }\n\
         two() { put \"$1\" 0 0 16; put \"$1\" 16 16 16; }\n\
         sparse() { put \"$1\" 0 0 2; put \"$1\" 8 8 2; }\n\
         odd() { dd if=source of=\"$1\" bs=5000 count=1 status=none; sync; }\n\
         prealloc() { fallocate -l 65536 \"$1\"; sync; }\n\
         kept() { put \"$1\" 0 0 2; fallocate -n -o 8192 -l 57344 \"$1\"; sync; }\n\
         reflink() { cp --reflink=always shared \"$1\"; sync; }\n",
    );
    for (n, k) in CASES.iter().enumerate() {
        for twin in ["a", "k"] {
            script.push_str(&format!("{} {twin}{n}\n", k.recipe));
        }
    }
    script.push_str("sync\n");
    script
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// A scratch volume, removed when it drops, including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(512 << 20))
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
        guest_kernel_write_ok(&image.to_string_lossy(), name, &make_script());
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

fn fallocate(fs: *mut fs_btrfs_fs, path: &str, mode: u32, offset: u64, len: u64) -> i32 {
    unsafe { fs_btrfs_fallocate(fs, c(path).as_ptr(), mode, offset, len) }
}

#[track_caller]
fn refused(fs: *mut fs_btrfs_fs, path: &str, mode: u32, offset: u64, len: u64, want: i32) {
    assert_eq!(
        fallocate(fs, path, mode, offset, len),
        -1,
        "fallocate {path} mode {mode} {offset}+{len} succeeded and should not have"
    );
    assert_eq!(
        fs_btrfs_last_errno(),
        want,
        "fallocate {path} mode {mode} {offset}+{len}: wrong errno for {:?}",
        last_error()
    );
}

fn read(fs: *mut fs_btrfs_fs, path: &str) -> Vec<u8> {
    let mut buf = vec![0u8; 2 << 20];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            c(path).as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert!(n >= 0, "read {path}: {}", last_error());
    buf.truncate(n as usize);
    buf
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

fn after(line: &str, word: &str) -> u64 {
    let mut it = line.split_whitespace();
    while let Some(w) = it.next() {
        if w == word {
            return it.next().unwrap().parse().unwrap();
        }
    }
    panic!("no {word} in {line:?}")
}

/// One `EXTENT_DATA` item: its key offset, type number, the extent's disk
/// address and length, its offset into the extent, and its length. A hole's
/// address is 0.
type RawItem = (u64, u64, u64, u64, u64, u64);

fn extent_items(fs_tree: &str, ino: u64) -> Vec<RawItem> {
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
        let kind = body
            .iter()
            .find(|l| l.contains("type"))
            .unwrap_or_else(|| panic!("inode {ino}'s item at {key_offset} has no type"));
        let disk = body
            .iter()
            .find(|l| l.contains(" data disk byte"))
            .unwrap_or_else(|| panic!("inode {ino}'s item at {key_offset} has no disk byte"));
        let within = body
            .iter()
            .find(|l| l.contains(" data offset"))
            .unwrap_or_else(|| panic!("inode {ino}'s item at {key_offset} has no offset"));
        items.push((
            key_offset,
            after(kind, "type"),
            after(disk, "byte"),
            after(disk, "nr"),
            after(within, "offset"),
            after(within, "nr"),
        ));
    }
    items
}

/// Inode `ino`'s extent items with each extent named by what it is: `shared N`
/// for an extent at one of the addresses `common` lists, the same for both
/// twins, and `own N` for the `N`th of the file's own, at a different address
/// in each.
fn item_shapes(items: &[RawItem], common: &[u64]) -> Vec<(u64, u64, String, u64, u64, u64)> {
    let mut own: Vec<u64> = Vec::new();
    items
        .iter()
        .map(|&(key, kind, bytenr, disk_len, within, len)| {
            let name = match common.iter().position(|b| *b == bytenr) {
                Some(n) => format!("shared {n}"),
                None if bytenr == 0 => "hole".to_string(),
                None => {
                    let n = own.iter().position(|b| *b == bytenr).unwrap_or_else(|| {
                        own.push(bytenr);
                        own.len() - 1
                    });
                    format!("own {n}")
                }
            };
            (key, kind, name, disk_len, within, len)
        })
        .collect()
}

/// The offset and count of every data reference naming inode `ino`, inline or
/// an item of its own: `dump-tree` prints both forms alike.
fn references_naming(extent_tree: &str, ino: u64) -> Vec<(u64, u64)> {
    let mut found: Vec<(u64, u64)> = extent_tree
        .lines()
        .filter(|l| l.contains("extent data backref root"))
        .filter(|l| after(l, "objectid") == ino)
        .map(|l| (after(l, "offset"), after(l, "count")))
        .collect();
    found.sort_unstable();
    found
}

/// The size, byte count and flags of inode `ino`'s item.
fn inode_fields(fs_tree: &str, ino: u64) -> (u64, u64, String) {
    let lines: Vec<&str> = fs_tree.lines().collect();
    for (at, line) in lines.iter().enumerate() {
        let Some((objectid, kind, _)) = item_key(line) else {
            continue;
        };
        if objectid != ino || kind != "INODE_ITEM" {
            continue;
        }
        let body = &lines[at + 1..];
        let sizes = body
            .iter()
            .take_while(|l| item_key(l).is_none())
            .find(|l| l.contains(" nbytes "))
            .unwrap_or_else(|| panic!("inode {ino} has no size line"));
        let flags = body
            .iter()
            .take_while(|l| item_key(l).is_none())
            .find(|l| l.contains(" flags "))
            .unwrap_or_else(|| panic!("inode {ino} has no flags line"));
        let flags = flags.split(" flags ").nth(1).unwrap().trim().to_string();
        return (after(sizes, "size"), after(sizes, "nbytes"), flags);
    }
    panic!("no inode item for {ino}")
}

/// The commands the kernel's twin is changed with.
fn kernel_changes() -> String {
    let mut script = String::from("cd \"$MNT\"\n");
    for (n, k) in CASES.iter().enumerate() {
        let flags = if k.mode & FS_BTRFS_FALLOC_PUNCH_HOLE != 0 {
            "-p"
        } else if k.mode & KEEP != 0 {
            "-n"
        } else {
            ""
        };
        script.push_str(&format!(
            "fallocate {flags} -o {} -l {} k{n}\n",
            k.offset, k.len
        ));
    }
    script.push_str("sync\n");
    script
}

/// Each named file's name, size, SHA-256 and inode, as the kernel reads them.
fn kernel_reads(image: &str, what: &str, names: &[String]) -> BTreeMap<String, (u64, String, u64)> {
    guest_kernel_read_ok(
        image,
        what,
        &format!(
            "cd \"$MNT\"\n\
             for f in {all}; do \
               printf '%s %s %s %s\\n' \"$f\" \"$(stat -c %s \"$f\")\" \
                 \"$(sha256sum \"$f\" | cut -d' ' -f1)\" \"$(stat -c %i \"$f\")\"; done",
            all = names.join(" ")
        ),
    )
    .lines()
    .map(|line| {
        let w: Vec<&str> = line.split_whitespace().collect();
        (
            w[0].to_string(),
            (
                w[1].parse().unwrap(),
                w[2].to_string(),
                w[3].parse().unwrap(),
            ),
        )
    })
    .collect()
}

/// Each file's modification and change times, as the kernel reads them.
fn times(image: &str, what: &str, names: &[String]) -> BTreeMap<String, String> {
    guest_kernel_read_ok(
        image,
        what,
        &format!("cd \"$MNT\"\nstat -c '%n %y %z' {}", names.join(" ")),
    )
    .lines()
    .map(|line| {
        let (name, stamps) = line.split_once(' ').unwrap();
        (name.to_string(), stamps.to_string())
    })
    .collect()
}

/// The logical byte ranges `filefrag -v` flags unwritten in each file, adjacent
/// ones joined: the kernel's report joins extents that happen to be adjacent on
/// disk, and where a twin's extents landed is not part of the comparison.
fn unwritten_ranges(image: &str, names: &[String]) -> BTreeMap<String, Vec<(u64, u64)>> {
    let report = guest_kernel_read_ok(
        image,
        "filefrag",
        &format!(
            "cd \"$MNT\"\nfor f in {}; do echo \"== $f\"; filefrag -b4096 -v \"$f\"; done",
            names.join(" ")
        ),
    );
    let mut out: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
    let mut name = String::new();
    for line in report.lines() {
        if let Some(n) = line.strip_prefix("== ") {
            name = n.to_string();
            out.entry(name.clone()).or_default();
            continue;
        }
        if !line.contains("unwritten") {
            continue;
        }
        let words: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == ':' || c == '.')
            .filter(|w| !w.is_empty())
            .collect();
        // ext, logical start, logical end, physical start, physical end, length.
        let start: u64 = words[1].parse().unwrap();
        let blocks: u64 = words[5].parse().unwrap();
        let (from, to) = (start * 4096, (start + blocks) * 4096);
        let ranges = out.get_mut(&name).unwrap();
        match ranges.last_mut() {
            Some(last) if last.1 == from => last.1 = to,
            _ => ranges.push((from, to)),
        }
    }
    out
}

/// A clean bill from `btrfs check`, data digests included, and a kernel scrub.
#[track_caller]
fn judged_clean(image: &str, when: &str) {
    assert_btrfs_check_clean(Path::new(image), when);
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum {when}:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(image, "scrub", "btrfs scrub start -B \"$MNT\" >/dev/null");
}

/// Hold every twin pair to one another: their bytes, inode fields, items,
/// references and unwritten ranges.
#[track_caller]
fn twins_agree(image: &str, when: &str, pairs: &[(String, String)]) {
    let names: Vec<String> = pairs
        .iter()
        .flat_map(|(a, k)| [a.clone(), k.clone()])
        .chain(["shared".to_string()])
        .collect();
    let seen = kernel_reads(image, when, &names);
    let fs_tree = dump_tree(Path::new(image), "5");
    let extents = dump_tree(Path::new(image), "extent");
    let unwritten = unwritten_ranges(image, &names);
    let common: Vec<u64> = extent_items(&fs_tree, seen["shared"].2)
        .iter()
        .map(|item| item.2)
        .collect();
    let by_address: BTreeSet<u64> = common.iter().copied().collect();
    assert_eq!(by_address.len(), 1, "/shared is not one extent: {common:?}");
    for (a, k) in pairs {
        let (ours, theirs) = (&seen[a], &seen[k]);
        assert_eq!(
            (ours.0, &ours.1),
            (theirs.0, &theirs.1),
            "{when}: the kernel reads /{a} and /{k} differently"
        );
        assert_eq!(
            inode_fields(&fs_tree, ours.2),
            inode_fields(&fs_tree, theirs.2),
            "{when}: /{a}'s inode is not the one the kernel left for /{k}"
        );
        assert_eq!(
            item_shapes(&extent_items(&fs_tree, ours.2), &common),
            item_shapes(&extent_items(&fs_tree, theirs.2), &common),
            "{when}: /{a}'s extent items are not the ones the kernel made for /{k}"
        );
        assert_eq!(
            references_naming(&extents, ours.2),
            references_naming(&extents, theirs.2),
            "{when}: the references naming /{a} are not the ones naming /{k}"
        );
        assert_eq!(
            unwritten[a], unwritten[k],
            "{when}: filefrag flags other bytes of /{a} unwritten than of /{k}"
        );
    }
}

#[test]
fn ranges_preallocated_and_punched_through_the_abi_are_what_the_kernel_makes() {
    let scratch = Scratch::new("fallocate-kernel");
    let image = scratch.image();
    let pairs: Vec<(String, String)> = (0..CASES.len())
        .map(|n| (format!("a{n}"), format!("k{n}")))
        .collect();
    let names: Vec<String> = pairs
        .iter()
        .flat_map(|(a, k)| [a.clone(), k.clone()])
        .collect();

    // The twins start the same, or nothing below means anything.
    twins_agree(&image, "before anything changed", &pairs);
    let before = times(&image, "before the changes", &names);

    let ro = unsafe { fs_btrfs_mount(c(&image).as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    refused(ro, "/a0", 0, 0, 4096, EROFS);
    refused(ro, "/a0", PUNCH, 0, 4096, EROFS);
    unsafe { fs_btrfs_umount(ro) };

    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    // What the kernel refuses is refused, and a miss names what is missing.
    refused(fs, "/missing", 0, 0, 4096, ENOENT);
    refused(fs, "/", 0, 0, 4096, EISDIR);
    refused(fs, "/a0", 0, 0, 0, ENOTSUP);
    refused(fs, "/a0", PUNCH, 0, 0, ENOTSUP);
    refused(fs, "/a0", 0, u64::MAX, 4096, ENOTSUP);
    refused(fs, "/a0", FS_BTRFS_FALLOC_PUNCH_HOLE, 0, 4096, ENOTSUP);
    refused(fs, "/a0", 0x10, 0, 4096, ENOTSUP);

    for (n, k) in CASES.iter().enumerate() {
        assert_eq!(
            fallocate(fs, &format!("/a{n}"), k.mode, k.offset, k.len),
            0,
            "case {n}, {}: {}",
            k.what,
            last_error()
        );
    }
    unsafe { fs_btrfs_umount(fs) };
    judged_clean(&image, "after ranges changed through the ABI");

    guest_kernel_write_ok(&image, "the kernel's own fallocate", &kernel_changes());
    judged_clean(&image, "after the kernel's own fallocate");
    twins_agree(&image, "after the same changes", &pairs);

    // Each case moved the times, or did not, as the kernel's did.
    let moved = times(&image, "after the changes", &names);
    for (n, k) in CASES.iter().enumerate() {
        let (a, t) = (format!("a{n}"), format!("k{n}"));
        assert_eq!(
            moved[&a] != before[&a],
            moved[&t] != before[&t],
            "case {n}, {}: the kernel and the ABI disagree on whether the times moved",
            k.what
        );
    }

    // The ABI reads what the kernel reads, and preallocated space reads zeros.
    let seen = kernel_reads(&image, "read back", &names);
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    for name in &names {
        let bytes = read(fs, &format!("/{name}"));
        assert_eq!(
            fs_btrfs_test_support::sha256_hex(&bytes),
            seen[name].1,
            "the ABI reads /{name} differently from the kernel"
        );
    }
    assert!(
        read(fs, "/a0").iter().all(|b| *b == 0),
        "preallocated space does not read as zeros"
    );

    // The unwritten flag is there at all, so comparing it means something.
    let unwritten = unwritten_ranges(&image, &["a0".to_string()]);
    assert_eq!(
        unwritten["a0"],
        vec![(0, 1024 * K)],
        "filefrag does not flag /a0 unwritten"
    );

    // Writing into what the ABI preallocated, by the ABI and by the kernel,
    // leaves what the kernel's own preallocation is left with: the inode's
    // preallocation flag is what sends the kernel's write to the same extent.
    let payload = vec![0x5a_u8; 8192];
    let written = unsafe {
        fs_btrfs_write_file(
            fs,
            c("/a0").as_ptr(),
            payload.as_ptr().cast::<c_void>(),
            4096,
            8192,
        )
    };
    assert_eq!(written, 8192, "write into /a0: {}", last_error());
    unsafe { fs_btrfs_umount(fs) };
    guest_kernel_write_ok(
        &image,
        "the kernel writes into preallocated space",
        "cd \"$MNT\"\n\
         head -c 8192 /dev/zero | tr '\\0' Z | dd of=k0 bs=4096 seek=1 conv=notrunc status=none\n\
         for f in a1 k1; do head -c 8192 /dev/zero | tr '\\0' Z | dd of=$f bs=4096 seek=2 conv=notrunc status=none; done\n\
         sync",
    );
    judged_clean(&image, "after writes into preallocated space");
    twins_agree(&image, "after writes into preallocated space", &pairs);

    // A snapshot then shares every leaf and extent: the ABI's punch must drop
    // references rather than release what the snapshot reads, and the same
    // change by the kernel must leave the same items.
    guest_kernel_write_ok(
        &image,
        "the kernel takes a snapshot",
        "btrfs subvolume snapshot \"$MNT\" \"$MNT/snap\" >/dev/null\nsync",
    );
    let snapped = kernel_reads(&image, "before the snapshot's sources change", &names);
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    assert_eq!(
        fallocate(fs, "/a2", PUNCH, 0, 64 * K),
        0,
        "a punch a snapshot shares: {}",
        last_error()
    );
    assert_eq!(
        fallocate(fs, "/a3", 0, 0, 128 * K),
        0,
        "a fallocate a snapshot shares: {}",
        last_error()
    );
    unsafe { fs_btrfs_umount(fs) };
    guest_kernel_write_ok(
        &image,
        "the kernel makes the same changes",
        "cd \"$MNT\"\nfallocate -p -o 0 -l 65536 k2\nfallocate -o 0 -l 131072 k3\nsync",
    );
    judged_clean(&image, "after changes under a snapshot");
    twins_agree(&image, "after changes under a snapshot", &pairs);
    let in_snapshot = guest_kernel_read_ok(
        &image,
        "the snapshot's files",
        &format!(
            "cd \"$MNT/snap\"\nfor f in {}; do printf '%s %s\\n' \"$f\" \"$(sha256sum \"$f\" | cut -d' ' -f1)\"; done",
            names.join(" ")
        ),
    );
    for line in in_snapshot.lines() {
        let (name, sum) = line.split_once(' ').unwrap();
        assert_eq!(
            sum, snapped[name].1,
            "/snap/{name} changed when its source did"
        );
    }

    // The kernel deletes all of it, which finds each reference left here.
    guest_kernel_write_ok(
        &image,
        "the kernel deletes the files",
        &format!(
            "cd \"$MNT\"\nrm {}\nbtrfs subvolume delete -c snap >/dev/null\nsync",
            names.join(" ")
        ),
    );
    assert_btrfs_check_clean(Path::new(&image), "after the kernel deleted the files");
}

/// Inline data is refused rather than preallocated around, and the file does
/// not change.
#[test]
fn a_file_holding_inline_data_is_refused() {
    let scratch = Scratch::new("fallocate-inline");
    let image = scratch.image();
    guest_kernel_write_ok(
        &image,
        "an inline file",
        "cd \"$MNT\"\nprintf 'short' > inline\nsync",
    );
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    refused(fs, "/inline", 0, 0, 1 << 20, ENOTSUP);
    refused(fs, "/inline", PUNCH, 0, 3, ENOTSUP);
    assert_eq!(read(fs, "/inline"), b"short", "/inline changed");
    unsafe { fs_btrfs_umount(fs) };
    assert_btrfs_check_clean(Path::new(&image), "after inline data was refused");
}
