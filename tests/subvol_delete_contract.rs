//! No-tool contracts for deleting a subvolume (#267). The kernel's
//! judgement of a delete lives in subvol_delete_kernel.rs.

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
        let image = PathBuf::from(temp_path!("subvol-delete-{tag}.img"));
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

    fn digest(&self) -> String {
        sha256_hex(&std::fs::read(&self.0).unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn a_deleted_subvolume_is_gone_after_a_remount() {
    let image = Scratch::new("gone");
    let keep = {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        let keep = fs.create_subvolume(root, b"keep", 0o755, 0, 0).unwrap();
        fs.create_subvolume(root, b"gone", 0o755, 0, 0).unwrap();
        let ro = fs.create_subvolume(root, b"ro", 0o755, 0, 0).unwrap();
        fs.set_subvolume_read_only(ro, true).unwrap();
        let size = fs.root_inode().unwrap().size;

        let before = fs.superblock().generation;
        fs.delete_subvolume(root, b"gone").expect("delete gone");
        assert_eq!(fs.superblock().generation, before + 1, "one transaction");
        // A read-only subvolume is deleted, as the kernel deletes a
        // read-only snapshot.
        fs.delete_subvolume(root, b"ro").expect("delete ro");
        assert_eq!(
            fs.root_inode().unwrap().size,
            size - 2 * (b"gone".len() + b"ro".len()) as u64,
            "the directory's size counts each name twice"
        );
        keep
    };

    let mut fs = image.mount();
    let root = fs.root_inode().unwrap().ino;
    let ids: Vec<u64> = fs.subvolumes().unwrap().iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        vec![FS_TREE_OBJECTID, keep],
        "a dead subvolume is listed"
    );
    assert_eq!(fs.subvolume_at(b"/keep"), Ok(keep));
    for path in ["/gone", "/ro"] {
        assert_eq!(fs.subvolume_at(path.as_bytes()), Err(Error::NotFound));
        assert!(fs.resolve_path(path).is_err(), "{path} still resolves");
    }
    let names: Vec<Vec<u8>> = fs
        .read_dir(root)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, vec![b"keep".to_vec()]);

    // The name is free again, for a subvolume with a new id.
    let again = fs.create_subvolume(root, b"gone", 0o755, 0, 0).unwrap();
    assert!(again > keep, "a dead subvolume's id was reused: {again}");
}

#[test]
fn a_refused_delete_writes_nothing() {
    let image = Scratch::new("refused");
    let kept = {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        let default = fs.create_subvolume(root, b"default", 0o755, 0, 0).unwrap();
        let kept = fs.create_subvolume(root, b"kept", 0o755, 0, 0).unwrap();
        fs.mkdir(root, b"plain", 0o755, 0, 0).unwrap();
        fs.create(root, b"file", 0o644, 0, 0).unwrap();
        fs.set_default_subvolume(default).unwrap();
        kept
    };
    let before = image.digest();

    {
        let mut fs = image.mount();
        let root = fs.root_inode().unwrap().ino;
        let generation = fs.superblock().generation;
        assert_eq!(fs.delete_subvolume(root, b"missing"), Err(Error::NotFound));
        assert_eq!(
            fs.delete_subvolume(root, b"plain"),
            Err(Error::NotFound),
            "an ordinary directory is no subvolume"
        );
        assert!(
            matches!(
                fs.delete_subvolume(root, b"default"),
                Err(Error::UnsupportedFeature(_))
            ),
            "the default subvolume is refused, as the kernel refuses it with EPERM"
        );
        let file = fs.lookup(root, b"file").unwrap().ino;
        assert_eq!(
            fs.delete_subvolume(file, b"kept"),
            Err(Error::NotADirectory)
        );
        let mut sv = fs.open_subvolume(kept).unwrap();
        let sv_root = sv.root_inode().unwrap().ino;
        assert!(
            sv.delete_subvolume(sv_root, b"kept").is_err(),
            "a handle on another subvolume deleted something"
        );
        assert_eq!(
            fs.superblock().generation,
            generation,
            "a refusal committed"
        );
    }
    {
        let dev = Arc::new(FileDevice::open(&image.0).unwrap());
        let mut fs = Filesystem::mount(dev).unwrap();
        let root = fs.root_inode().unwrap().ino;
        assert_eq!(fs.delete_subvolume(root, b"kept"), Err(Error::ReadOnly));
    }
    assert_eq!(image.digest(), before, "a refused delete changed the image");
}
