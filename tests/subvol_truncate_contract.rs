//! A truncate keeps its handle on the chosen tree across every commit (#519).
//! Independent on-disk validation lives in subvol_truncate_kernel.rs.

use fs_btrfs::{mkfs, Error, Filesystem};
use fs_btrfs_test_support::temp_path;
use fs_core::FileDevice;
use std::{path::PathBuf, sync::Arc};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn truncate_keeps_the_selected_subvolume_through_shrink_and_recursive_growth() {
    let image = Scratch(PathBuf::from(temp_path!("subvol-truncate-scope.img")));
    std::fs::create_dir_all(image.0.parent().unwrap()).unwrap();
    std::fs::File::create(&image.0)
        .and_then(|f| f.set_len(128 << 20))
        .unwrap();
    let dev = Arc::new(FileDevice::open_rw(&image.0).unwrap());
    mkfs::format(dev.as_ref(), &mkfs::Options::default()).unwrap();
    let mut top = Filesystem::mount_rw(dev).unwrap();
    let root = top.lookup_path("/").unwrap().ino;
    let top_ino = top.create(root, b"top-file", 0o644, 0, 0).unwrap();
    top.write(top_ino, 0, b"held at top").unwrap();
    let id = top.create_subvolume(root, b"sv", 0o755, 0, 0).unwrap();
    let mut fs = top.open_subvolume(id).unwrap();
    let ino = fs.create(root, b"data", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, b"abcdefgh").unwrap();
    fs.truncate(ino, 5).expect("shrink inside sv");
    assert_eq!(fs.subvolume_at(b"/").unwrap(), id);
    assert_eq!(fs.read_file(ino).unwrap(), b"abcde");
    fs.truncate(ino, 9000).expect("grow inline data inside sv");
    assert_eq!(fs.subvolume_at(b"/").unwrap(), id);
    let mut want = vec![0; 9000];
    want[..5].copy_from_slice(b"abcde");
    assert_eq!(fs.read_file(ino).unwrap(), want);
    fs.truncate(ino, 5000).unwrap();
    fs.truncate(ino, 12000)
        .expect("zero the partial last sector");
    want.resize(12000, 0);
    assert_eq!(fs.read_file(ino).unwrap(), want);
    fs.truncate(ino, 0).expect("release sv's data extent");
    assert!(fs.read_file(ino).unwrap().is_empty());
    assert_eq!(fs.subvolume_at(b"/").unwrap(), id);
    drop((fs, top));

    let top = Filesystem::mount(Arc::new(FileDevice::open(&image.0).unwrap())).unwrap();
    assert_eq!(top.read_file(top_ino).unwrap(), b"held at top");
    assert!(top.lookup_path("/data").is_err());
    let mut ro = top.open_subvolume(id).unwrap();
    assert!(matches!(ro.truncate(ino, 1), Err(Error::ReadOnly)));
    assert!(matches!(ro.truncate(ino, 0), Err(Error::ReadOnly)));
}
