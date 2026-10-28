//! A snapshot taken by this crate, judged by btrfs-progs and the kernel
//! (#267).
//!
//! On a volume this crate formats and fills -- an inline file, two files
//! with extents of their own, and a subvolume `sub` holding a third --
//! this crate snapshots the top level as `snap` and `sub` as the
//! read-only `rosnap`, each as one committed transaction. Then:
//!
//! - `btrfs check` must find the volume clean;
//! - the kernel must list both with `btrfs subvolume list -s`, and
//!   `btrfs subvolume show` must give each its source's UUID as its
//!   parent UUID, and `rosnap` the read-only flag;
//! - every file in each snapshot must hash as its source's does;
//! - the kernel must write into `snap` and into the top level, copying
//!   the blocks the two share, with each side keeping its own bytes, and
//!   refuse a write into `rosnap`;
//! - after the kernel's writes the volume must still be clean, and this
//!   crate must read each side's bytes back as the kernel left them.

use std::sync::Arc;

use fs_btrfs::fs::Filesystem;
use fs_btrfs::mkfs;
use fs_btrfs::subvol::FS_TREE_OBJECTID;
use fs_btrfs_test_support::{assert_btrfs_check_clean, guest_kernel_write_ok};
use fs_core::{BlockDevice, FileDevice};

/// Bytes that are not all one value, so a misplaced block shows.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// The value after `field:` on the line of `out` that starts with
/// `prefix` and holds it.
fn field<'a>(out: &'a str, prefix: &str, field: &str) -> &'a str {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix(prefix))
        .find_map(|l| l.trim().strip_prefix(field))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no {prefix}{field} line in the kernel's output:\n{out}"))
}

/// The digest `sha256sum` printed for `name` on a line tagged `tag`.
fn digest<'a>(out: &'a str, tag: &str, name: &str) -> &'a str {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix(tag))
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|w| w.len() == 2 && w[1].trim_start_matches('*') == name)
        .map(|w| w[0])
        .unwrap_or_else(|| panic!("no {tag}digest for {name} in the kernel's output:\n{out}"))
}

#[test]
fn a_snapshot_is_one_the_kernel_reads_and_writes() {
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "subvol-snapshot-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("snapshot.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .expect("create the image");

    let big = pattern(192 << 10, 7);
    {
        let dev = FileDevice::open_rw(&image).expect("open rw");
        mkfs::format(&dev, &mkfs::Options::default()).expect("format");
    }
    {
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open rw"));
        let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
        let root = fs.root_inode().expect("the top directory").ino;
        let small = fs.create(root, b"small", 0o644, 0, 0).unwrap();
        fs.write(small, 0, b"inline bytes").unwrap();
        let ino = fs.create(root, b"big", 0o644, 0, 0).unwrap();
        fs.write(ino, 0, &big).unwrap();
        let other = fs.create(root, b"other", 0o644, 0, 0).unwrap();
        fs.write(other, 0, &pattern(64 << 10, 99)).unwrap();
        let sub = fs.create_subvolume(root, b"sub", 0o755, 0, 0).unwrap();
        {
            let mut sv = fs.open_subvolume(sub).unwrap();
            let sv_root = sv.root_inode().unwrap().ino;
            let inside = sv.create(sv_root, b"inside", 0o644, 0, 0).unwrap();
            sv.write(inside, 0, &pattern(80 << 10, 3)).unwrap();
        }
        // `sv` committed after `fs` did, so `fs` is stale (#599): mount
        // the volume afresh before snapshotting through it.
        drop(fs);
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open rw"));
        let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");

        let generation = fs.superblock().generation;
        fs.create_snapshot(FS_TREE_OBJECTID, root, b"snap", false)
            .expect("snapshot the top level");
        fs.create_snapshot(sub, root, b"rosnap", true)
            .expect("snapshot sub read-only");
        assert_eq!(
            fs.superblock().generation,
            generation + 2,
            "one transaction each"
        );
    }

    assert_btrfs_check_clean(&image, "after the snapshots were taken");

    let image_str = image.to_str().expect("a UTF-8 scratch path").to_string();
    let out = guest_kernel_write_ok(
        &image_str,
        "the snapshots",
        r#"
cd "$MNT"
btrfs subvolume list -s . | sed 's/^/list /'
btrfs subvolume show . | sed 's/^/top /'
btrfs subvolume show sub | sed 's/^/sub /'
btrfs subvolume show snap | sed 's/^/snap /'
btrfs subvolume show rosnap | sed 's/^/rosnap /'
btrfs property get -ts rosnap ro | sed 's/^/rosnap-prop /'
sha256sum small big other | sed 's/^/top-sum /'
(cd snap && sha256sum small big other) | sed 's/^/snap-sum /'
sha256sum sub/inside | sed 's/^/sub-sum /'
sha256sum rosnap/inside | sed 's/^/rosnap-sum /'
printf 'snapside' | dd of=snap/big bs=1 seek=4096 conv=notrunc status=none
printf 'top-side' | dd of=big bs=1 seek=4096 conv=notrunc status=none
sync
if printf 'x' > rosnap/new 2>/dev/null; then echo rosnap-written; else echo rosnap-refused; fi
"#,
    );

    let listed: Vec<&str> = out
        .lines()
        .filter_map(|l| l.trim().strip_prefix("list "))
        .filter_map(|l| l.split_whitespace().last())
        .collect();
    for name in ["snap", "rosnap"] {
        assert!(
            listed.contains(&name),
            "the kernel does not list {name} as a snapshot:\n{out}"
        );
    }
    assert!(!listed.contains(&"sub"), "sub listed as a snapshot:\n{out}");

    let top_uuid = field(&out, "top ", "UUID:");
    let sub_uuid = field(&out, "sub ", "UUID:");
    assert_eq!(field(&out, "snap ", "Parent UUID:"), top_uuid, "{out}");
    assert_eq!(field(&out, "rosnap ", "Parent UUID:"), sub_uuid, "{out}");
    assert_ne!(field(&out, "snap ", "UUID:"), top_uuid, "{out}");
    assert_eq!(field(&out, "rosnap-prop ", "ro="), "true", "{out}");

    for name in ["small", "big", "other"] {
        assert_eq!(
            digest(&out, "snap-sum ", name),
            digest(&out, "top-sum ", name),
            "{name} in the snapshot:\n{out}"
        );
    }
    assert_eq!(
        digest(&out, "rosnap-sum ", "rosnap/inside"),
        digest(&out, "sub-sum ", "sub/inside"),
        "inside in the read-only snapshot:\n{out}"
    );
    assert!(
        out.lines().any(|l| l.trim() == "rosnap-refused"),
        "the kernel wrote into the read-only snapshot:\n{out}"
    );

    assert_btrfs_check_clean(&image, "after the kernel wrote into both sides");

    let dev = Arc::new(FileDevice::open(&image).expect("open"));
    let fs = Filesystem::mount(dev).expect("mount");
    let read = |fs: &Filesystem| {
        let root = fs.root_inode().unwrap().ino;
        fs.read_file(fs.lookup(root, b"big").unwrap().ino).unwrap()
    };
    let snap = fs.subvolume_at(b"/snap").expect("the snapshot");
    let mut top_big = big.clone();
    top_big[4096..4104].copy_from_slice(b"top-side");
    let mut snap_big = big;
    snap_big[4096..4104].copy_from_slice(b"snapside");
    assert!(
        read(&fs) == top_big,
        "the top level's big after the kernel's write"
    );
    assert!(
        read(&fs.open_subvolume(snap).unwrap()) == snap_big,
        "the snapshot's big after the kernel's write"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
