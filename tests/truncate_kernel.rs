//! Files truncated through the C ABI, judged by what is not this crate
//! (#262).
//!
//! A kernel-made volume holds a checksummed file of three extents, a
//! single-extent file, an inline file, an empty file and a reflinked pair.
//! Through the ABI one file is cut inside its second extent (the third is
//! released, the second referenced less), one is cut to nothing (its
//! extent released), the inline one is cut short, the empty one grown, and
//! one of the reflinked pair cut to nothing (one reference of its shared
//! extent dropped, the extent kept for the other) — each call one
//! committed transaction. Then `btrfs check` inspects the
//! whole volume: an extent still recorded but no longer referenced, a
//! free-space tree or block group `used` that disagrees, digests left for
//! a released extent, or an inode whose `nbytes` does not match its items
//! are each reported there. `btrfs check --check-data-csum` compares every
//! data sector left with its digest, the kernel mounts the volume and
//! reports each file's size and SHA-256, and a kernel scrub verifies every
//! sector. Last, the kernel writes to and truncates the same files and
//! `btrfs check` looks again.
//!
//! A second volume is made without the `NO_HOLES` feature, where growing a
//! file has to record its new range as hole items (#420). A third holds
//! files that end partway through a sector with stale bytes past the end,
//! grown so the rest of that sector reads as zeros (#421).

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path,
};
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

/// A scratch volume, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        Self::made(
            name,
            "no-holes",
            "cd \"$MNT\"\n\
             head -c 65536 /dev/urandom > multi\n\
             sync\n\
             head -c 65536 /dev/urandom >> multi\n\
             sync\n\
             head -c 65536 /dev/urandom >> multi\n\
             sync\n\
             head -c 300000 /dev/urandom > big\n\
             printf 'hello, inline world' > small\n\
             touch empty\n\
             head -c 8192 /dev/urandom > shared\n\
             cp --reflink=always shared shared2\n\
             mkdir dir\n\
             sync",
        )
        .0
    }

    /// A volume made with mkfs `features`, on which the guest kernel runs
    /// `script`; and what the script printed.
    fn made(name: &str, features: &str, script: &str) -> (Self, String) {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "16384", "-O", features])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        let said = guest_kernel_write_ok(&image.to_string_lossy(), name, script);
        (Scratch(dir), said)
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

fn truncate(fs: *mut fs_btrfs_fs, path: &str, size: u64) -> i32 {
    unsafe { fs_btrfs_truncate(fs, c(path).as_ptr(), size) }
}

#[track_caller]
fn refused(fs: *mut fs_btrfs_fs, path: &str, size: u64, want: i32) {
    assert_eq!(
        truncate(fs, path, size),
        -1,
        "truncate {path} {size} succeeded and should not have"
    );
    assert_eq!(
        fs_btrfs_last_errno(),
        want,
        "truncate {path} {size}: wrong errno for {:?}",
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

#[test]
fn files_truncated_through_the_abi_are_clean_and_the_kernel_agrees() {
    let scratch = Scratch::new("truncate-kernel");
    let image = scratch.image();
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    let multi = read(fs, "/multi", 1 << 20);
    assert_eq!(multi.len(), 196_608, "/multi is not the size made");
    let shared = read(fs, "/shared", 1 << 20);
    assert_eq!(shared.len(), 8192, "/shared is not the size made");

    // Inside the second of three extents: the third is released, the
    // second referenced only up to the sector holding the new end.
    assert_eq!(truncate(fs, "/multi", 70_000), 0, "{}", last_error());
    // To nothing: the one extent released.
    assert_eq!(truncate(fs, "/big", 0), 0, "{}", last_error());
    // Inline data cut short.
    assert_eq!(truncate(fs, "/small", 5), 0, "{}", last_error());
    // An empty file grown: the new range is an implicit hole.
    assert_eq!(truncate(fs, "/empty", 1 << 20), 0, "{}", last_error());
    // The same length: nothing changes.
    assert_eq!(truncate(fs, "/shared", 8192), 0, "{}", last_error());
    // One of a reflinked pair to nothing: its reference to the extent the
    // pair shares is dropped, and the extent stays for the other (#262).
    assert_eq!(truncate(fs, "/shared", 0), 0, "{}", last_error());

    assert_eq!(read(fs, "/multi", 1 << 20), &multi[..70_000]);
    assert!(read(fs, "/shared", 1 << 20).is_empty());
    assert!(
        read(fs, "/shared2", 1 << 20) == shared,
        "/shared2 changed when its reflinked twin was truncated"
    );
    assert_eq!(read(fs, "/small", 64), b"hello");
    assert_eq!(read(fs, "/empty", 2 << 20), vec![0u8; 1 << 20]);

    // Each refusal names its errno and writes nothing.
    let before = std::fs::read(&image).unwrap();
    // Growing inline data means moving it into an extent (#262).
    refused(fs, "/small", 100, ENOTSUP);
    refused(fs, "/dir", 0, EISDIR);
    refused(fs, "/missing", 0, ENOENT);
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused truncate wrote to the image"
    );
    unsafe { fs_btrfs_umount(fs) };

    let ro = unsafe { fs_btrfs_mount(c(&image).as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    refused(ro, "/multi", 0, EROFS);
    unsafe { fs_btrfs_umount(ro) };
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a read-only handle wrote to the image"
    );

    assert_btrfs_check_clean(
        Path::new(&image),
        "after files were truncated through the ABI",
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

    let seen = guest_kernel_read_ok(
        &image,
        "after the ABI's truncates",
        "cd \"$MNT\"\n\
         for f in multi big small empty shared shared2; do \
           printf '%s %s %s\\n' \"$f\" \"$(stat -c %s \"$f\")\" \
             \"$(sha256sum \"$f\" | cut -d' ' -f1)\"; done",
    );
    let want: String = [
        ("multi", multi[..70_000].to_vec()),
        ("big", Vec::new()),
        ("small", b"hello".to_vec()),
        ("empty", vec![0u8; 1 << 20]),
        ("shared", Vec::new()),
        ("shared2", shared.clone()),
    ]
    .iter()
    .map(|(name, bytes)| format!("{name} {} {}\n", bytes.len(), sha256_hex(bytes)))
    .collect();
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(
        lines.join("\n") + "\n",
        want,
        "the kernel sees the truncated files differently"
    );

    // Every sector the kernel can reach, against its digest.
    guest_kernel_write_ok(&image, "scrub", "btrfs scrub start -B \"$MNT\" >/dev/null");

    // The kernel writes to and truncates the same files, and the checker
    // looks again.
    guest_kernel_write_ok(
        &image,
        "the kernel changes the truncated files",
        "cd \"$MNT\"\n\
         head -c 5000 /dev/urandom >> multi\n\
         truncate -s 4096 big\n\
         echo more >> small\n\
         head -c 4096 /dev/urandom | dd of=empty bs=4096 seek=10 conv=notrunc status=none\n\
         truncate -s 1000 multi\n\
         sync",
    );
    assert_btrfs_check_clean(
        Path::new(&image),
        "after the kernel changed the truncated files",
    );
}

/// The `EXTENT_DATA` items `dump-tree` lists for inode `ino` in the
/// top-level subvolume, as `(file offset, disk bytenr, num_bytes)`.
fn dumped_extents(image: &Path, ino: u64) -> Vec<(u64, u64, u64)> {
    let fs_tree = dump_tree(image, "5");
    let lines: Vec<&str> = fs_tree.lines().collect();
    let needle = format!("key ({ino} EXTENT_DATA ");
    let mut items = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(start) = line.find(&needle) else {
            continue;
        };
        let offset = line[start + needle.len()..]
            .split(')')
            .next()
            .and_then(|o| o.parse().ok())
            .unwrap_or_else(|| panic!("no offset in {line:?}"));
        let body: Vec<&str> = lines[at + 1..]
            .iter()
            .take_while(|l| !l.contains(" key ("))
            .copied()
            .collect();
        let field = |phrase: &str, word: &str| -> u64 {
            let found = body
                .iter()
                .find(|l| l.contains(phrase))
                .unwrap_or_else(|| panic!("inode {ino}'s item at {offset} has no {phrase:?}"));
            let mut words = found.split_whitespace();
            words.find(|w| *w == word);
            words
                .next()
                .and_then(|w| w.parse().ok())
                .unwrap_or_else(|| panic!("inode {ino}'s item at {offset}: no {word} in {found:?}"))
        };
        items.push((
            offset,
            field("extent data disk byte", "byte"),
            field("extent data offset", "nr"),
        ));
    }
    items
}

/// Without the `NO_HOLES` feature, every range of a file up to its size,
/// rounded to a sector, must be covered by an `EXTENT_DATA` item, a hole
/// being one whose disk bytenr is 0 (#420). A kernel-made volume without
/// the feature holds an empty file, a file of two sectors and one the
/// kernel already grew past its data; each is grown through the ABI, one
/// of them twice. `btrfs check` reports a range left without an item as a
/// file extent discount; `dump-tree` must show the items tiling each file
/// exactly, everything past the old end a hole; the kernel reports each
/// file's size and SHA-256, writes into the new ranges, and `btrfs check`
/// looks again.
#[test]
fn files_grown_without_no_holes_are_covered_by_hole_items_and_the_kernel_agrees() {
    let (scratch, said) = Scratch::made(
        "truncate-holes",
        "^no-holes",
        "cd \"$MNT\"\n\
         touch empty\n\
         head -c 8192 /dev/urandom > aligned\n\
         head -c 8192 /dev/urandom > holey\n\
         truncate -s 16384 holey\n\
         sync\n\
         stat -c '%n %i' empty aligned holey",
    );
    let image = scratch.image();
    let ino = |name: &str| -> u64 {
        said.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
            .unwrap_or_else(|| panic!("the guest printed no inode for {name}: {said:?}"))
    };
    let super_block = oracle("btrfs")
        .args(["inspect-internal", "dump-super"])
        .arg(&image)
        .output();
    assert!(super_block.status.success(), "dump-super failed");
    assert!(
        !String::from_utf8_lossy(&super_block.stdout).contains("NO_HOLES"),
        "mkfs made the volume with the no-holes feature, so this test proves nothing"
    );
    assert_btrfs_check_clean(Path::new(&image), "the kernel-made volume");

    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    let aligned = read(fs, "/aligned", 1 << 20);
    assert_eq!(aligned.len(), 8192, "/aligned is not the size made");
    let holey = read(fs, "/holey", 1 << 20);
    assert_eq!(holey.len(), 16384, "/holey is not the size made");

    let grown = [
        ("empty", 1 << 20, Vec::new()),
        ("aligned", 65536, aligned),
        ("holey", 40960, holey),
    ];
    assert_eq!(truncate(fs, "/empty", 1 << 20), 0, "{}", last_error());
    assert_eq!(truncate(fs, "/aligned", 32768), 0, "{}", last_error());
    assert_eq!(truncate(fs, "/aligned", 65536), 0, "{}", last_error());
    assert_eq!(truncate(fs, "/holey", 40960), 0, "{}", last_error());
    for (name, size, data) in &grown {
        let mut want = data.clone();
        want.resize(*size, 0);
        assert!(
            read(fs, &format!("/{name}"), 2 << 20) == want,
            "/{name} does not read back as its data followed by zeros"
        );
    }
    unsafe { fs_btrfs_umount(fs) };

    assert_btrfs_check_clean(
        Path::new(&image),
        "after files were grown through the ABI without no-holes",
    );
    for (name, size, data) in &grown {
        let items = dumped_extents(Path::new(&image), ino(name));
        let mut end = 0;
        for &(offset, bytenr, num) in &items {
            assert_eq!(
                offset, end,
                "{name}'s items leave [{end}, {offset}) uncovered: {items:?}"
            );
            if offset >= data.len() as u64 {
                assert_eq!(
                    bytenr, 0,
                    "{name}'s item at {offset} is not a hole: {items:?}"
                );
            }
            end = offset + num;
        }
        assert_eq!(
            end, *size as u64,
            "{name}'s items do not end at its new size: {items:?}"
        );
    }

    let seen = guest_kernel_read_ok(
        &image,
        "after the ABI grew files without no-holes",
        "cd \"$MNT\"\n\
         for f in empty aligned holey; do \
           printf '%s %s %s\\n' \"$f\" \"$(stat -c %s \"$f\")\" \
             \"$(sha256sum \"$f\" | cut -d' ' -f1)\"; done",
    );
    let want: String = grown
        .iter()
        .map(|(name, size, data)| {
            let mut bytes = data.clone();
            bytes.resize(*size, 0);
            format!("{name} {size} {}\n", sha256_hex(&bytes))
        })
        .collect();
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(
        lines.join("\n") + "\n",
        want,
        "the kernel sees the grown files differently"
    );

    guest_kernel_write_ok(
        &image,
        "the kernel writes into the grown ranges",
        "cd \"$MNT\"\n\
         head -c 4096 /dev/urandom | dd of=empty bs=4096 seek=10 conv=notrunc status=none\n\
         head -c 4096 /dev/urandom | dd of=aligned bs=4096 seek=9 conv=notrunc status=none\n\
         head -c 5000 /dev/urandom >> holey\n\
         sync",
    );
    assert_btrfs_check_clean(
        Path::new(&image),
        "after the kernel wrote into the grown ranges",
    );
}

/// Files that end partway through a sector whose last sector still holds
/// bytes past the end — one written and then cut short by the kernel, two
/// cut short through the ABI, which keeps the extent whole — are grown
/// through the ABI, inside that sector and past it. The rest of the old
/// last sector must read as zeros (#421): `btrfs check` and
/// `--check-data-csum` call the volume clean, the kernel reports each
/// file's size and SHA-256 with zeros from the old end, and a scrub passes.
#[test]
fn files_ending_partway_through_a_sector_grow_with_zeros_from_their_old_end() {
    let (scratch, _) = Scratch::made(
        "truncate-grow-mid-sector",
        "no-holes",
        "cd \"$MNT\"\n\
         head -c 10000 /dev/urandom > tail\n\
         sync\n\
         truncate -s 6000 tail\n\
         head -c 10000 /dev/urandom > cut\n\
         head -c 10000 /dev/urandom > cut2\n\
         sync",
    );
    let image = scratch.image();
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    let tail = read(fs, "/tail", 1 << 20);
    assert_eq!(tail.len(), 6000, "/tail is not the size made");
    let cut = read(fs, "/cut", 1 << 20);
    let cut2 = read(fs, "/cut2", 1 << 20);
    assert_eq!(truncate(fs, "/cut", 6000), 0, "{}", last_error());
    assert_eq!(truncate(fs, "/cut2", 5000), 0, "{}", last_error());

    // Past the old last sector, and inside it.
    let grown = [
        ("tail", 50_000usize, &tail[..6000]),
        ("cut", 7000, &cut[..6000]),
        ("cut2", 30_000, &cut2[..5000]),
    ];
    let want: Vec<(&str, Vec<u8>)> = grown
        .iter()
        .map(|(name, size, kept)| {
            let mut bytes = kept.to_vec();
            bytes.resize(*size, 0);
            (*name, bytes)
        })
        .collect();
    for (name, size, _) in &grown {
        let at = format!("/{name}");
        assert_eq!(
            truncate(fs, &at, *size as u64),
            0,
            "{name}: {}",
            last_error()
        );
    }
    for (name, bytes) in &want {
        assert!(
            read(fs, &format!("/{name}"), 1 << 20) == *bytes,
            "/{name} does not read as its kept bytes and then zeros"
        );
    }
    unsafe { fs_btrfs_umount(fs) };

    assert_btrfs_check_clean(
        Path::new(&image),
        "after files ending partway through a sector were grown",
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

    let seen = guest_kernel_read_ok(
        &image,
        "after growing files that end partway through a sector",
        "cd \"$MNT\"\n\
         for f in tail cut cut2; do \
           printf '%s %s %s\\n' \"$f\" \"$(stat -c %s \"$f\")\" \
             \"$(sha256sum \"$f\" | cut -d' ' -f1)\"; done",
    );
    let expected: String = want
        .iter()
        .map(|(name, bytes)| format!("{name} {} {}\n", bytes.len(), sha256_hex(bytes)))
        .collect();
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(
        lines.join("\n") + "\n",
        expected,
        "the kernel sees the grown files differently"
    );

    guest_kernel_write_ok(&image, "scrub", "btrfs scrub start -B \"$MNT\" >/dev/null");
}
