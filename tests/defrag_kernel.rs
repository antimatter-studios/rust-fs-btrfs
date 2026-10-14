//! Files defragmented through the library, judged by what is not this
//! crate (#305).
//!
//! The kernel writes a volume whose files are fragmented the way real ones
//! are: appended to a little at a time, in separate transactions, with
//! another file appended to between each, so no two of a file's extents
//! are neighbours on disk. One of them is reflinked whole to a second
//! name, and one file is written in one go. Through the library, the
//! fragmented file and the reflinked one are defragmented, and the file
//! that is already one extent is left untouched.
//!
//! # The oracles
//!
//! `btrfs check` inspects the whole volume: a copy recorded with the
//! wrong references, an old extent left recorded, a free-space tree or a
//! block group `used` that disagrees, or digests left for an extent given
//! up are each reported there. `btrfs check --check-data-csum` compares
//! every data sector with its digest. `dump-tree` counts each file's
//! `EXTENT_DATA` items, and the kernel mounts the volume and reports each
//! file's size, SHA-256 and `filefrag`'s count of its extents: the bytes
//! the kernel reads are the bytes it wrote, now in one extent. A kernel
//! scrub verifies every sector, and the kernel then writes to the
//! defragmented files and `btrfs check` looks again. Last, once the kernel
//! has snapshotted the volume, a defragment is refused and writes nothing.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::Error;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path,
};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Nine extents: eight 64 KiB appends, then 12,345 bytes, so the file
/// ends partway through a sector.
const FRAG: &str = "frag";
/// Four 32 KiB appends, then reflinked whole to [`SHARED2`].
const SHARED: &str = "shared";
const SHARED2: &str = "shared2";
/// Written in one go: one extent.
const WHOLE: &str = "whole";
/// Appended to between every append to the others, so none of their
/// extents is next to another of the same file.
const OTHER: &str = "other";

const FILES: [&str; 5] = [FRAG, SHARED, SHARED2, WHOLE, OTHER];

/// The kernel's fill.
const FILL: &str = "cd \"$MNT\"\n\
     for i in 1 2 3 4 5 6 7 8; do\n\
       head -c 65536 /dev/urandom >> frag\n\
       head -c 65536 /dev/urandom >> other\n\
       sync\n\
     done\n\
     head -c 12345 /dev/urandom >> frag\n\
     head -c 4096 /dev/urandom >> other\n\
     sync\n\
     for i in 1 2 3 4; do\n\
       head -c 32768 /dev/urandom >> shared\n\
       head -c 4096 /dev/urandom >> other\n\
       sync\n\
     done\n\
     cp --reflink=always shared shared2\n\
     head -c 100000 /dev/urandom > whole\n\
     mkdir dir\n\
     sync";

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
        guest_kernel_write_ok(&image.to_string_lossy(), name, FILL);
        Scratch(dir)
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open read-write"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write")
}

fn ino_of(fs: &Filesystem, name: &str) -> u64 {
    fs.lookup_path(&format!("/{name}"))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .ino
}

/// What the kernel says about each file: its size, its SHA-256 and how
/// many extents `filefrag` finds.
fn kernel_view(image: &Path, what: &str) -> BTreeMap<String, (u64, String, u64)> {
    let script: String = FILES
        .iter()
        .map(|name| {
            format!(
                "printf '%s %s %s %s\\n' {name} \"$(stat -c %s \"$MNT/{name}\")\" \
                 \"$(sha256sum \"$MNT/{name}\" | cut -d' ' -f1)\" \
                 \"$(filefrag \"$MNT/{name}\" | awk '{{print $2}}')\"\n"
            )
        })
        .collect();
    guest_kernel_read_ok(&image.to_string_lossy(), what, &script)
        .lines()
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(words.len(), 4, "[{what}] the kernel said {line:?}");
            let size = words[1].parse().unwrap_or_else(|_| panic!("{line:?}"));
            let extents = words[3].parse().unwrap_or_else(|_| panic!("{line:?}"));
            (words[0].to_string(), (size, words[2].to_string(), extents))
        })
        .collect()
}

/// How many `EXTENT_DATA` items `dump-tree` lists for inode `ino` in the
/// top-level subvolume.
fn dumped_items(image: &Path, ino: u64) -> usize {
    let needle = format!("key ({ino} EXTENT_DATA ");
    dump_tree(image, "5").matches(&needle).count()
}

#[test]
fn defragmented_files_are_one_extent_and_the_checker_and_kernel_agree() {
    let scratch = Scratch::new("defrag-kernel");
    let image = scratch.image();
    let before = kernel_view(&image, "before the defragment");
    assert_eq!(before.len(), FILES.len(), "{before:?}");
    for name in [FRAG, SHARED, OTHER] {
        assert!(
            before[name].2 > 1,
            "the kernel made {name} as {} extent(s); the fill no longer fragments it",
            before[name].2
        );
    }

    let mut fs = mount_rw(&image);
    let (frag, shared, shared2, whole) = (
        ino_of(&fs, FRAG),
        ino_of(&fs, SHARED),
        ino_of(&fs, SHARED2),
        ino_of(&fs, WHOLE),
    );
    let dumped_before = [frag, shared, shared2].map(|ino| dumped_items(&image, ino));
    let bytes: BTreeMap<u64, Vec<u8>> = [frag, shared, shared2, whole]
        .iter()
        .map(|&ino| (ino, fs.read_file(ino).unwrap()))
        .collect();

    // A file that is one extent already: nothing to do, and nothing
    // committed.
    let untouched = std::fs::read(&image).unwrap();
    let done = fs.defragment(whole).unwrap();
    assert_eq!((done.runs, done.items_before, done.items_after), (0, 1, 1));
    assert!(
        std::fs::read(&image).unwrap() == untouched,
        "defragmenting a file of one extent wrote to the image"
    );

    // The fragmented file: every extent into one.
    let done = fs.defragment(frag).unwrap();
    assert_eq!(done.runs, 1, "{done:?}");
    assert_eq!(done.items_before, dumped_before[0], "{done:?}");
    assert_eq!(done.items_after, 1, "{done:?}");
    assert_eq!(done.bytes, 8 * 65536 + 16384, "{done:?}");

    // The reflinked file: its own copy, its references to the shared
    // extents dropped, and its twin left reading them.
    let done = fs.defragment(shared).unwrap();
    assert_eq!((done.runs, done.items_after), (1, 1), "{done:?}");

    for (&ino, want) in &bytes {
        assert!(
            &fs.read_file(ino).unwrap() == want,
            "inode {ino} reads differently after the defragment"
        );
    }
    let dir = ino_of(&fs, "dir");
    assert!(matches!(fs.defragment(dir), Err(Error::NotAFile)));
    assert!(matches!(
        fs.defragment_to(frag, 1),
        Err(Error::UnsupportedFeature(_))
    ));
    drop(fs);

    let dev = Arc::new(FileDevice::open(&image).expect("open read-only"));
    let mut ro = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount read-only");
    assert!(matches!(ro.defragment(frag), Err(Error::ReadOnly)));
    drop(ro);

    assert_btrfs_check_clean(&image, "after files were defragmented");
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
    assert_eq!(dumped_items(&image, frag), 1, "dump-tree: {FRAG}");
    assert_eq!(dumped_items(&image, shared), 1, "dump-tree: {SHARED}");
    assert_eq!(
        dumped_items(&image, shared2),
        dumped_before[2],
        "dump-tree: {SHARED2} lost or gained items when its twin was defragmented"
    );

    let after = kernel_view(&image, "after the defragment");
    for name in FILES {
        let (b, a) = (&before[name], &after[name]);
        assert_eq!(
            (a.0, &a.1),
            (b.0, &b.1),
            "the kernel reads {name} differently after the defragment"
        );
    }
    assert_eq!(after[FRAG].2, 1, "filefrag: {FRAG}");
    assert_eq!(after[SHARED].2, 1, "filefrag: {SHARED}");
    assert_eq!(after[SHARED2].2, before[SHARED2].2, "filefrag: {SHARED2}");
    assert_eq!(after[WHOLE].2, before[WHOLE].2, "filefrag: {WHOLE}");
    let sha = |ino: u64| sha256_hex(&bytes[&ino]);
    assert_eq!(after[FRAG].1, sha(frag), "the kernel reads {FRAG}");

    // Every sector the kernel can reach, against its digest.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );

    // The kernel changes the defragmented files, and the checker looks
    // again; then it snapshots the volume.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel changes the defragmented files",
        "cd \"$MNT\"\n\
         head -c 5000 /dev/urandom >> frag\n\
         head -c 4096 /dev/urandom | dd of=shared bs=4096 seek=3 conv=notrunc status=none\n\
         rm shared2\n\
         sync\n\
         btrfs subvolume snapshot \"$MNT\" \"$MNT/snap\" >/dev/null\n\
         sync",
    );
    assert_btrfs_check_clean(&image, "after the kernel changed the defragmented files");

    // A fragmented file whose extents a snapshot may be reading: refused,
    // and nothing written.
    let mut fs = mount_rw(&image);
    let other = ino_of(&fs, OTHER);
    let snapshotted = std::fs::read(&image).unwrap();
    match fs.defragment(other) {
        Err(Error::UnsupportedFeature(why)) => assert!(why.contains("snapshot"), "{why}"),
        other => panic!("defragmenting a snapshotted file was not refused: {other:?}"),
    }
    drop(fs);
    assert!(
        std::fs::read(&image).unwrap() == snapshotted,
        "a refused defragment wrote to the image"
    );
}
