//! An in-place write refuses an extent a snapshot still reads, even when
//! the extent tree counts one reference (#63).
//!
//! `write_at` took `refs == 1` to mean the extent belongs to one file. A
//! snapshot of a subvolume whose tree is taller than a leaf does not raise
//! that count: `btrfs_copy_root` adds references to the blocks the root
//! points at, which are tree blocks, not data. So the snapshot reads the
//! extent and the count still says one. The kernel's own nocow check treats
//! an extent from a generation at or before the tree's `last_snapshot` as
//! shared.
//!
//! The image is `snapshot/btrfs-nodatacow-snapshot.img` from
//! `scripts/build-nodatacow-fixtures.sh`: a `chattr +C` file, then a
//! read-only snapshot of the top-level subvolume, taken by the kernel. The
//! test first checks with btrfs-progs that the case is real, meaning the
//! file's extent does read `refs 1`. It then asks the driver to overwrite
//! the file and requires a refusal and an image unchanged byte for byte.
//! Skips when the fixture is missing, unless `BTRFS_ORACLE_FIXTURES=required`.

use fs_btrfs::fs::Filesystem;
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const FILE: &str = "/nc/inplace.bin";

fn fixture() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".vm-share")
        .join("snapshot")
        .join("btrfs-nodatacow-snapshot.img");
    if p.exists() {
        return Some(p);
    }
    assert!(
        std::env::var("BTRFS_ORACLE_FIXTURES").as_deref() != Ok("required"),
        "BTRFS_ORACLE_FIXTURES=required, but {} is missing",
        p.display()
    );
    eprintln!("skip: {} not built", p.display());
    None
}

fn dump_tree(image: &Path, tree: &str) -> String {
    let out = Command::new("btrfs")
        .args(["inspect-internal", "dump-tree", "-t", tree])
        .arg(image)
        .output()
        .expect("btrfs-progs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The number after `word` in `line`.
fn after(line: &str, word: &str) -> u64 {
    let mut it = line.split_whitespace();
    while let Some(w) = it.next() {
        if w == word {
            return it.next().unwrap().parse().unwrap();
        }
    }
    panic!("no {word} in {line:?}")
}

#[test]
fn a_write_under_a_snapshot_is_refused_though_the_extent_counts_one_reference() {
    let Some(source) = fixture() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("btrfs-snapshot-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("snapshot.img");
    std::fs::copy(&source, &image).unwrap();

    let ino = {
        let dev = Arc::new(FileDevice::open(&image).unwrap());
        let fs = Filesystem::mount(dev).expect("mount");
        let snap = fs
            .subvolumes()
            .unwrap()
            .into_iter()
            .find(|s| s.name == b"snap")
            .expect("the fixture's snapshot");
        assert!(snap.is_snapshot, "`snap` is a snapshot");
        fs.lookup_path(FILE).expect("the nodatacow file").ino
    };

    // THE CASE IS REAL: the file's extent reads one reference, as the
    // extent tree the kernel wrote records it, although the snapshot
    // reads it too.
    let fs_tree = dump_tree(&image, "5");
    let lines: Vec<&str> = fs_tree.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains(&format!("key ({ino} EXTENT_DATA 0)")))
        .expect("the file's first extent");
    let bytenr = after(
        lines[at + 2..]
            .iter()
            .find(|l| l.contains("disk byte"))
            .unwrap(),
        "byte",
    );
    let extents = dump_tree(&image, "extent");
    let lines: Vec<&str> = extents.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains(&format!("key ({bytenr} EXTENT_ITEM ")))
        .expect("the extent's item");
    assert_eq!(
        after(lines[at + 1], "refs"),
        1,
        "the fixture no longer holds the case: the snapshot raised the \
         reference count, so a refusal here would prove nothing new"
    );

    let before = std::fs::read(&image).unwrap();
    {
        let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
        assert!(
            !fs.can_write_in_place(ino).unwrap(),
            "reported writable in place although the snapshot reads it"
        );
        let got = fs.write_at(ino, 0, &[0xa5; 4096]);
        assert!(
            got.is_err(),
            "overwrote an extent the snapshot still reads: {got:?}"
        );
    }
    assert!(
        std::fs::read(&image).unwrap() == before,
        "the refused write changed the image"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
