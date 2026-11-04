//! The edges of preallocating and punching a range through the C ABI (#713):
//! what `fallocate_kernel.rs` does not reach, and what is refused.
//!
//! As there, twin files start from one recipe, one is changed through the ABI
//! and the other by the kernel's own `fallocate`, and `dump-tree` must show the
//! same items and inode fields for both, with a clean `btrfs check`.

#![allow(unsafe_code)]

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};

const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

const K: u64 = 1024;
const KEEP: u32 = FS_BTRFS_FALLOC_KEEP_SIZE;
const PUNCH: u32 = FS_BTRFS_FALLOC_PUNCH_HOLE | FS_BTRFS_FALLOC_KEEP_SIZE;

/// One change: the recipe both twins start from, then `fallocate(2)`'s mode,
/// offset and length.
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

const CASES: [Case; 14] = [
    // A preallocation past a file ending partway through a sector rewrites
    // that sector, whatever holds it.
    case(
        "past a partial last sector that is preallocated",
        "partial_prealloc",
        0,
        0,
        64 * K,
    ),
    case(
        "past a partial last sector in a hole",
        "partial_hole",
        0,
        0,
        64 * K,
    ),
    case(
        "clear of a partial last sector in a hole, size kept",
        "partial_hole",
        KEEP,
        8 * K,
        8 * K,
    ),
    // A punch's edges in holes.
    case(
        "from inside a hole to inside data",
        "sparse",
        PUNCH,
        10 * K,
        25 * K,
    ),
    case(
        "from inside data to inside a hole",
        "sparse",
        PUNCH,
        5 * K,
        5 * K,
    ),
    // Past the size rounded up, a punch zeros nothing.
    case(
        "inside one sector past the size",
        "kept",
        PUNCH,
        21 * K,
        2 * K,
    ),
    case("off sectors past the size", "kept", PUNCH, 21 * K, 10 * K),
    // A preallocated item a punch covers whole keeps its place.
    case(
        "inside the one sector of a preallocated item",
        "prealloc4",
        PUNCH,
        100,
        200,
    ),
    case(
        "off sectors over all of a preallocated item",
        "prealloc8",
        PUNCH,
        K,
        4 * K,
    ),
    // Written neighbours over one extent join the sector written between them.
    case(
        "between sectors of a preallocated extent already written",
        "written_around",
        PUNCH,
        4 * K + 100,
        200,
    ),
    // Compressed extents are cut, and a sector of one copied out plain.
    case("through a compressed extent", "zlib", PUNCH, 16 * K, 16 * K),
    case(
        "off sectors in a compressed extent",
        "zlib",
        PUNCH,
        K,
        8 * K,
    ),
    // A nodatacow file takes both, with whole sectors of a punch.
    case("a nodatacow file", "nocow", 0, 0, 64 * K),
    case(
        "whole sectors of a nodatacow file",
        "nocow_data",
        PUNCH,
        16 * K,
        16 * K,
    ),
];

fn make_script() -> String {
    let mut script = String::from(
        "cd \"$MNT\"\n\
         head -c 65536 /dev/urandom > source\n\
         sync\n\
         put() { dd if=source of=\"$1\" bs=4096 skip=\"$2\" seek=\"$3\" count=\"$4\" conv=notrunc status=none; sync; }\n\
         sparse() { put \"$1\" 0 0 2; put \"$1\" 8 8 2; }\n\
         kept() { put \"$1\" 0 0 2; fallocate -n -o 8192 -l 57344 \"$1\"; sync; }\n\
         partial_prealloc() { fallocate -l 6144 \"$1\"; sync; }\n\
         partial_hole() { truncate -s 5000 \"$1\"; sync; }\n\
         prealloc4() { fallocate -l 4096 \"$1\"; sync; }\n\
         prealloc8() { fallocate -l 8192 \"$1\"; sync; }\n\
         written_around() { fallocate -l 16384 \"$1\"; sync; dd if=source of=\"$1\" bs=4096 count=1 conv=notrunc status=none; dd if=source of=\"$1\" bs=4096 seek=2 count=1 conv=notrunc status=none; sync; }\n\
         zlib() { : > \"$1\"; btrfs property set \"$1\" compression zlib; head -c 65536 /dev/zero | dd of=\"$1\" bs=65536 status=none; sync; }\n\
         nocow() { : > \"$1\"; chattr +C \"$1\"; sync; }\n\
         nocow_data() { nocow \"$1\"; put \"$1\" 0 0 16; }\n",
    );
    for (n, k) in CASES.iter().enumerate() {
        for twin in ["a", "k"] {
            script.push_str(&format!("{} {twin}{n}\n", k.recipe));
        }
    }
    script.push_str("sync\n");
    script
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
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

fn word_after(line: &str, word: &str) -> u64 {
    let mut it = line.split_whitespace();
    while let Some(w) = it.next() {
        if w == word {
            return it.next().unwrap().parse().unwrap();
        }
    }
    panic!("no {word} in {line:?}")
}

/// What `dump-tree` says of inode `ino`: its size, byte count and flags, then
/// each extent item's key offset, type, on-disk length, offset into the extent
/// and length, with the extent named by its place among the file's own, since
/// where a twin's extents landed is not part of the comparison.
fn shape(fs_tree: &str, ino: u64) -> Vec<String> {
    let mut out = Vec::new();
    let mut own: Vec<u64> = Vec::new();
    let lines: Vec<&str> = fs_tree.lines().collect();
    let prefix = format!("key ({ino} ");
    for (at, line) in lines.iter().enumerate() {
        if !line.trim_start().starts_with("item ") || !line.contains(&prefix) {
            continue;
        }
        let body = lines[at + 1..]
            .iter()
            .take_while(|l| !l.trim_start().starts_with("item "));
        if line.contains("INODE_ITEM") {
            let body: Vec<&str> = body.copied().collect();
            let sizes = body.iter().find(|l| l.contains(" nbytes ")).unwrap();
            let flags = body.iter().find(|l| l.contains(" flags ")).unwrap();
            out.push(format!(
                "size {} nbytes {} flags {}",
                word_after(sizes, "size"),
                word_after(sizes, "nbytes"),
                flags.split(" flags ").nth(1).unwrap().trim()
            ));
        } else if line.contains("EXTENT_DATA") {
            let body: Vec<&str> = body.copied().collect();
            let key = line.split("EXTENT_DATA ").nth(1).unwrap();
            let key = key.split(')').next().unwrap();
            let kind = body.iter().find(|l| l.contains(" type ")).unwrap();
            let disk = body.iter().find(|l| l.contains(" data disk byte")).unwrap();
            let within = body.iter().find(|l| l.contains(" data offset")).unwrap();
            let bytenr = word_after(disk, "byte");
            let name = own.iter().position(|b| *b == bytenr).unwrap_or_else(|| {
                own.push(bytenr);
                own.len() - 1
            });
            out.push(format!(
                "at {key} {} extent {name} disk {} offset {} len {}",
                kind.split(" type ").nth(1).unwrap().trim(),
                word_after(disk, "nr"),
                word_after(within, "offset"),
                word_after(within, "nr"),
            ));
        }
    }
    out
}

/// Each file's inode number and SHA-256, as the kernel reads them.
fn kernel_reads(image: &str, names: &[String]) -> Vec<(String, u64, String)> {
    guest_kernel_read_ok(
        image,
        "the kernel reads the twins",
        &format!(
            "cd \"$MNT\"\nfor f in {}; do printf '%s %s %s\\n' \"$f\" \"$(stat -c %i \"$f\")\" \
             \"$(sha256sum \"$f\" | cut -d' ' -f1)\"; done",
            names.join(" ")
        ),
    )
    .lines()
    .map(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        (w[0].to_string(), w[1].parse().unwrap(), w[2].to_string())
    })
    .collect()
}

#[track_caller]
fn digests_clean(image: &str, when: &str) {
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
}

#[test]
fn the_edges_of_a_range_leave_what_the_kernel_leaves() {
    let scratch = Scratch::new("fallocate-edges");
    let image = scratch.image();
    guest_kernel_write_ok(&image, "the twins", &make_script());

    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
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
    digests_clean(&image, "after the ABI's changes");

    let mut kernel = String::from("cd \"$MNT\"\n");
    for (n, k) in CASES.iter().enumerate() {
        let flags = if k.mode & FS_BTRFS_FALLOC_PUNCH_HOLE != 0 {
            "-p"
        } else if k.mode & KEEP != 0 {
            "-n"
        } else {
            ""
        };
        kernel.push_str(&format!(
            "fallocate {flags} -o {} -l {} k{n}\n",
            k.offset, k.len
        ));
    }
    kernel.push_str("sync\n");
    guest_kernel_write_ok(&image, "the kernel's own fallocate", &kernel);
    digests_clean(&image, "after the kernel's own fallocate");

    let names: Vec<String> = (0..CASES.len())
        .flat_map(|n| [format!("a{n}"), format!("k{n}")])
        .collect();
    let seen = kernel_reads(&image, &names);
    let fs_tree = dump_tree(Path::new(&image), "5");
    for (n, k) in CASES.iter().enumerate() {
        let (a, t) = (&seen[n * 2], &seen[n * 2 + 1]);
        assert_eq!(
            a.2, t.2,
            "case {n}, {}: the kernel reads the twins differently",
            k.what
        );
        assert_eq!(
            shape(&fs_tree, a.1),
            shape(&fs_tree, t.1),
            "case {n}, {}: /{} is not what the kernel left for /{}",
            k.what,
            a.0,
            t.0
        );
    }

    // The kernel deletes all of it, which finds each reference left here.
    guest_kernel_write_ok(
        &image,
        "the kernel deletes the files",
        &format!("cd \"$MNT\"\nrm {}\nsync", names.join(" ")),
    );
    assert_btrfs_check_clean(Path::new(&image), "after the kernel deleted the files");
}

#[test]
fn ranges_on_a_selected_subvolume_match_the_kernel_and_preserve_its_snapshot() {
    let scratch = Scratch::new("fallocate-subvolume");
    let image = scratch.image();
    let shared = case("a snapshotted regular extent", "", PUNCH, K, 8 * K);
    let cases: Vec<&Case> = CASES.iter().chain([&shared]).collect();
    let shared_index = CASES.len();
    guest_kernel_write_ok(
        &image,
        "the selected subvolume's twins and snapshot",
        &format!(
            "btrfs subvolume create \"$MNT/sv\" >/dev/null\n\
             head -c 65536 /dev/urandom > \"$MNT/top-file\"\n\
             cp --reflink=never \"$MNT/top-file\" \"$MNT/sv/a{shared_index}\"\n\
             cp --reflink=never \"$MNT/top-file\" \"$MNT/sv/k{shared_index}\"\n\
             sync\nbtrfs subvolume snapshot -r \"$MNT/sv\" \"$MNT/snap\" >/dev/null\n{}",
            make_script().replacen("cd \"$MNT\"", "cd \"$MNT/sv\"", 1)
        ),
    );
    let ids = guest_kernel_read_ok(
        &image,
        "the selected subvolume and snapshot IDs",
        "btrfs subvolume list \"$MNT\" | awk '$NF == \"sv\" { print $2 }'\n\
         btrfs subvolume list \"$MNT\" | awk '$NF == \"snap\" { print $2 }'",
    );
    let ids: Vec<u64> = ids.lines().map(|line| line.parse().unwrap()).collect();
    assert_eq!(ids.len(), 2);
    let preserved = vec![
        "top-file".to_string(),
        format!("snap/a{shared_index}"),
        format!("snap/k{shared_index}"),
    ];
    let before = kernel_reads(&image, &preserved);
    let snapshot_before = dump_tree(Path::new(&image), &ids[1].to_string());

    let top = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!top.is_null(), "mount_rw: {}", last_error());
    let selected = unsafe { fs_btrfs_open_subvolume(top, ids[0]) };
    assert!(!selected.is_null(), "open_subvolume: {}", last_error());
    for (index, change) in cases.iter().enumerate() {
        assert_eq!(
            fallocate(
                selected,
                &format!("/a{index}"),
                change.mode,
                change.offset,
                change.len
            ),
            0,
            "subvolume case {index}, {}: {}",
            change.what,
            last_error()
        );
    }
    unsafe {
        fs_btrfs_umount(selected);
        fs_btrfs_umount(top);
    }
    digests_clean(&image, "after the selected handle's range changes");

    let mut kernel = String::from("cd \"$MNT/sv\"\n");
    for (index, change) in cases.iter().enumerate() {
        let flags = if change.mode & FS_BTRFS_FALLOC_PUNCH_HOLE != 0 {
            "-p"
        } else if change.mode & KEEP != 0 {
            "-n"
        } else {
            ""
        };
        kernel.push_str(&format!(
            "fallocate {flags} -o {} -l {} k{index}\n",
            change.offset, change.len
        ));
    }
    kernel.push_str("sync\n");
    guest_kernel_write_ok(
        &image,
        "the kernel changes the subvolume's controls",
        &kernel,
    );
    digests_clean(&image, "after the kernel's subvolume range changes");

    let names: Vec<String> = (0..cases.len())
        .flat_map(|index| [format!("sv/a{index}"), format!("sv/k{index}")])
        .collect();
    let seen = kernel_reads(&image, &names);
    let tree = dump_tree(Path::new(&image), &ids[0].to_string());
    let extents = dump_tree(Path::new(&image), "extent");
    let references = |ino| {
        // Matched as text: `dump-tree` names the top level's root FS_TREE, not 5.
        let reference = format!("extent data backref root {} objectid {ino} ", ids[0]);
        let mut found: Vec<(u64, u64)> = extents
            .lines()
            .filter(|line| line.contains(&reference))
            .map(|line| (word_after(line, "offset"), word_after(line, "count")))
            .collect();
        found.sort_unstable();
        found
    };
    let mut compare = String::from("cd \"$MNT\"\n");
    for (index, change) in cases.iter().enumerate() {
        let (ours, control) = (&seen[index * 2], &seen[index * 2 + 1]);
        assert_eq!(
            ours.2, control.2,
            "{}: the kernel reads different bytes",
            change.what
        );
        assert_eq!(
            shape(&tree, ours.1),
            shape(&tree, control.1),
            "{}",
            change.what
        );
        assert_eq!(references(ours.1), references(control.1), "{}", change.what);
        compare.push_str(&format!("cmp sv/a{index} sv/k{index}\n"));
    }
    compare.push_str(&format!(
        "cmp top-file snap/a{shared_index}\ncmp top-file snap/k{shared_index}\n"
    ));
    guest_kernel_read_ok(
        &image,
        "the kernel compares every byte of the twins",
        &compare,
    );
    assert_eq!(kernel_reads(&image, &preserved), before);
    let snapshot_after = dump_tree(Path::new(&image), &ids[1].to_string());
    for (_, ino, _) in &before[1..] {
        assert_eq!(shape(&snapshot_before, *ino), shape(&snapshot_after, *ino));
    }

    let top = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!top.is_null(), "remount_rw: {}", last_error());
    let snapshot = unsafe { fs_btrfs_open_subvolume(top, ids[1]) };
    assert!(!snapshot.is_null(), "open snapshot: {}", last_error());
    for mode in [0, KEEP, PUNCH] {
        assert_eq!(
            fallocate(snapshot, &format!("/a{shared_index}"), mode, 0, 4096),
            -1
        );
        assert_eq!(fs_btrfs_last_errno(), 30, "{}", last_error());
    }
    unsafe {
        fs_btrfs_umount(snapshot);
        fs_btrfs_umount(top);
    }
    digests_clean(
        &image,
        "after refusing range changes on the read-only snapshot",
    );
}

/// What is refused changes nothing: the bytes read the same, and the checker
/// has nothing to say.
#[test]
fn what_is_not_written_yet_is_refused_and_changes_nothing() {
    let scratch = Scratch::new("fallocate-edges-refused");
    let image = scratch.image();
    guest_kernel_write_ok(
        &image,
        "files to refuse",
        "cd \"$MNT\"\n\
         head -c 65536 /dev/urandom > nocow_source\n\
         : > nocow\nchattr +C nocow\ncat nocow_source > nocow\n\
         fallocate -l 65536 reflinked\ncp --reflink=always reflinked other\n\
         fallocate -l 65536 snapped\n\
         sync\nbtrfs subvolume snapshot \"$MNT\" \"$MNT/snap\" >/dev/null\n\
         btrfs subvolume create \"$MNT/sub\" >/dev/null\n\
         fallocate -l 65536 \"$MNT/sub/file\"\n\
         cp --reflink=always \"$MNT/sub/file\" \"$MNT/sub/other\"\nsync",
    );
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    for (path, why) in [
        ("/nocow", "part of a sector of a nodatacow file's data"),
        ("/reflinked", "part of a sector another file refers to"),
        ("/snapped", "part of a sector a snapshot refers to"),
    ] {
        let before = read(fs, path);
        assert_eq!(fallocate(fs, path, PUNCH, K, 8 * K), -1, "{why}");
        assert_eq!(fs_btrfs_last_errno(), ENOTSUP, "{why}: {}", last_error());
        assert_eq!(read(fs, path), before, "{why}: the bytes changed");
    }

    // More space than the volume holds, without asking the allocator for it.
    assert_eq!(fallocate(fs, "/other", 0, 0, 1 << 62), -1);
    assert_eq!(fs_btrfs_last_errno(), ENOTSUP, "{}", last_error());
    // More than any data block group holds in one piece.
    assert_eq!(fallocate(fs, "/other", 0, 1 << 30, 400 << 20), -1);
    assert_eq!(fs_btrfs_last_errno(), ENOTSUP, "{}", last_error());

    // The subvolume's handle refuses what the top level refuses.
    let ids = guest_kernel_read_ok(
        &image,
        "the subvolume's id",
        "btrfs subvolume list \"$MNT\" | awk '$NF == \"sub\" { print $2 }'",
    );
    let id: u64 = ids.trim().parse().unwrap();
    let sub = unsafe { fs_btrfs_open_subvolume(fs, id) };
    assert!(!sub.is_null(), "open_subvolume: {}", last_error());
    let before = read(sub, "/file");
    assert_eq!(fallocate(sub, "/file", PUNCH, K, 8 * K), -1);
    assert_eq!(fs_btrfs_last_errno(), ENOTSUP, "{}", last_error());
    assert_eq!(read(sub, "/file"), before);
    unsafe {
        fs_btrfs_umount(sub);
        fs_btrfs_umount(fs);
    }
    digests_clean(&image, "after everything was refused");
}
