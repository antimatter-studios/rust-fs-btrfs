//! No-tool contracts for snapshotting a subvolume (#267). The kernel's
//! judgement of a snapshot lives in subvol_snapshot_kernel.rs.

use fs_btrfs::error::Error;
use fs_btrfs::subvol::FS_TREE_OBJECTID;
use fs_btrfs::{mkfs, Filesystem};
use fs_btrfs_test_support::{sha256_hex, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::path::PathBuf;
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let image = PathBuf::from(temp_path!("subvol-snapshot-{tag}.img"));
        std::fs::create_dir_all(image.parent().unwrap()).unwrap();
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(128 << 20))
            .unwrap();
        let dev = FileDevice::open_rw(&image).unwrap();
        mkfs::format(&dev, &mkfs::Options::default()).unwrap();
        Self(image)
    }

    fn mount(&self) -> Filesystem {
        let dev = Arc::new(FileDevice::open_rw(&self.0).unwrap());
        Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap()
    }

    fn mount_ro(&self) -> Filesystem {
        let dev = Arc::new(FileDevice::open(&self.0).unwrap());
        Filesystem::mount(dev).unwrap()
    }

    fn digest(&self) -> String {
        sha256_hex(&std::fs::read(&self.0).unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bytes that are not all one value, so a misplaced block shows.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Every name in the top directory of `fs`, with each regular file's
/// contents.
fn contents(fs: &Filesystem) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let root = fs.root_inode().unwrap().ino;
    let mut out: Vec<_> = fs
        .read_dir(root)
        .unwrap()
        .into_iter()
        .map(|e| {
            let inode = fs.read_inode(e.ino).unwrap();
            let data = (!inode.is_dir()).then(|| fs.read_file(e.ino).unwrap());
            (e.name, data)
        })
        .collect();
    out.sort();
    out
}

/// A volume whose top level holds an inline file, two files with extents
/// of their own, one sharing an extent with the other through a clone,
/// and a directory; and a subvolume `sub` holding one file.
fn populated(tag: &str) -> (Scratch, u64) {
    let image = Scratch::new(tag);
    let sub = {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        let small = fs.create(root, b"small", 0o644, 0, 0).unwrap();
        fs.write(small, 0, b"inline bytes").unwrap();
        let big = fs.create(root, b"big", 0o644, 0, 0).unwrap();
        fs.write(big, 0, &pattern(192 << 10, 7)).unwrap();
        let other = fs.create(root, b"other", 0o644, 0, 0).unwrap();
        fs.write(other, 0, &pattern(64 << 10, 99)).unwrap();
        fs.mkdir(root, b"dir", 0o755, 0, 0).unwrap();
        let sub = fs.create_subvolume(root, b"sub", 0o755, 0, 0).unwrap();
        let mut sv = fs.open_subvolume(sub).unwrap();
        let sv_root = sv.root_inode().unwrap().ino;
        let inside = sv.create(sv_root, b"inside", 0o644, 0, 0).unwrap();
        sv.write(inside, 0, &pattern(80 << 10, 3)).unwrap();
        sub
    };
    (image, sub)
}

#[test]
fn a_snapshot_of_the_top_level_reads_as_its_source() {
    let (image, _) = populated("top");
    let source = contents(&image.mount_ro());
    let snap = {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        let generation = fs.superblock().generation;
        let snap = fs
            .create_snapshot(FS_TREE_OBJECTID, root, b"snap", false)
            .expect("snapshot the top level");
        assert_eq!(
            fs.superblock().generation,
            generation + 1,
            "one transaction"
        );
        snap
    };

    let fs = image.mount_ro();
    let listed = fs.subvolumes().unwrap();
    let s = listed
        .iter()
        .find(|s| s.id == snap)
        .expect("the snapshot is listed");
    assert_eq!(s.path, "snap");
    assert_eq!(s.parent, FS_TREE_OBJECTID);
    assert!(s.is_snapshot, "the snapshot is not marked one: {s:?}");
    assert!(!s.read_only, "a writable snapshot is read-only: {s:?}");
    let top = listed.iter().find(|s| s.id == FS_TREE_OBJECTID).unwrap();
    assert_eq!(
        top.last_snapshot, s.generation,
        "the source's last_snapshot is not the snapshot's transaction"
    );
    let (top_uuid, _) = fs.subvolume_uuids(FS_TREE_OBJECTID).unwrap();
    let (snap_uuid, parent_uuid) = fs.subvolume_uuids(snap).unwrap();
    assert_eq!(parent_uuid, top_uuid, "the snapshot's parent UUID");
    assert_ne!(snap_uuid, top_uuid, "the snapshot reuses its source's UUID");
    assert_ne!(snap_uuid, [0; 16], "the snapshot has no UUID");

    // The snapshot holds the source as it stood: its own name is not in it.
    let taken = fs.open_subvolume(snap).unwrap();
    assert_eq!(contents(&taken), source, "the snapshot's files");
    assert_eq!(fs.subvolume_at(b"/snap"), Ok(snap));
}

#[test]
fn a_read_only_snapshot_of_a_subvolume_reads_as_its_source() {
    let (image, sub) = populated("sub");
    let source = contents(&image.mount_ro().open_subvolume(sub).unwrap());
    let snap = {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        let dir = fs.lookup(root, b"dir").unwrap().ino;
        fs.create_snapshot(sub, dir, b"rosnap", true)
            .expect("snapshot sub")
    };

    let fs = image.mount_ro();
    let s = fs
        .subvolumes()
        .unwrap()
        .into_iter()
        .find(|s| s.id == snap)
        .expect("the snapshot is listed");
    assert_eq!(s.path, "dir/rosnap");
    assert!(s.is_snapshot && s.read_only, "{s:?}");
    assert_eq!(
        fs.subvolume_uuids(snap).unwrap().1,
        fs.subvolume_uuids(sub).unwrap().0,
        "the snapshot's parent UUID"
    );
    assert_eq!(
        contents(&fs.open_subvolume(snap).unwrap()),
        source,
        "the snapshot's files"
    );

    // A read-only snapshot refuses a write; its source still takes one.
    let fs = image.mount();
    let mut ro = fs.open_subvolume(snap).unwrap();
    let ro_root = ro.root_inode().unwrap().ino;
    assert!(ro.create(ro_root, b"new", 0o644, 0, 0).is_err());
    let mut sv = fs.open_subvolume(sub).unwrap();
    let sv_root = sv.root_inode().unwrap().ino;
    let inside = sv.lookup(sv_root, b"inside").unwrap().ino;
    sv.write(inside, 0, b"changed in the source").unwrap();
    let fs = image.mount_ro();
    assert_eq!(
        contents(&fs.open_subvolume(snap).unwrap()),
        source,
        "a write to the source reached the snapshot"
    );
}

#[test]
fn a_refused_snapshot_writes_nothing() {
    let (image, sub) = populated("refused");
    {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        fs.create_snapshot(sub, root, b"taken", false).unwrap();
    }
    let before = image.digest();

    {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        let generation = fs.superblock().generation;
        assert_eq!(
            fs.create_snapshot(4242, root, b"unknown", false),
            Err(Error::NotFound),
            "an unknown source"
        );
        assert!(
            matches!(
                fs.create_snapshot(sub, root, b"taken", false),
                Err(Error::UnsupportedFeature(_))
            ),
            "a name already present"
        );
        assert!(
            matches!(
                fs.create_snapshot(sub, root, b"dir", false),
                Err(Error::UnsupportedFeature(_))
            ),
            "a directory's name"
        );
        let file = fs.lookup(root, b"big").unwrap().ino;
        assert_eq!(
            fs.create_snapshot(sub, file, b"x", false),
            Err(Error::NotADirectory)
        );
        assert_eq!(
            fs.superblock().generation,
            generation,
            "a refusal committed"
        );
    }
    {
        let mut fs = image.mount_ro();
        let root = fs.root_inode().unwrap().ino;
        assert_eq!(
            fs.create_snapshot(sub, root, b"ro", false),
            Err(Error::ReadOnly)
        );
    }
    assert_eq!(image.digest(), before, "a refused snapshot wrote something");
}

#[test]
fn a_source_tree_taller_than_a_leaf_is_refused_by_name() {
    let image = Scratch::new("tall");
    {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        // Enough inodes that the top level's tree outgrows one leaf.
        for i in 0..200 {
            let name = format!("file-{i:04}");
            fs.create(root, name.as_bytes(), 0o644, 0, 0).unwrap();
        }
    }
    let before = image.digest();
    {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        match fs.create_snapshot(FS_TREE_OBJECTID, root, b"snap", false) {
            Err(Error::UnsupportedFeature(why)) => {
                assert!(why.contains("level"), "the refusal does not name it: {why}")
            }
            other => panic!("a tree taller than a leaf: {other:?}"),
        }
    }
    assert_eq!(image.digest(), before, "a refused snapshot wrote something");
}
