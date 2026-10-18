//! No-tool contracts for a namespace handle's tree scope (#518).
//! The independent on-disk validation lives in subvol_namespace_kernel.rs.

use fs_btrfs::{capi::*, mkfs, Filesystem};
use fs_btrfs_test_support::{sha256_hex, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let image = PathBuf::from(temp_path!("subvol-namespace-{tag}.img"));
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn namespace_mutations_keep_the_selected_subvolume_after_every_commit() {
    let image = Scratch::new("scope");
    let mut top = image.mount();
    let root = top.lookup_path("/").unwrap().ino;
    top.create(root, b"top-file", 0o644, 0, 0).unwrap();
    let id = top.create_subvolume(root, b"sv", 0o755, 0, 0).unwrap();
    let mut fs = top.open_subvolume(id).unwrap();
    let root = fs.lookup_path("/").unwrap().ino;
    let ino = fs.create(root, b"source", 0o644, 0, 0).unwrap();
    fs.mkdir(root, b"empty", 0o755, 0, 0).unwrap();
    fs.link(ino, root, b"linked").expect("link inside sv");
    fs.rename(root, b"linked", root, b"renamed")
        .expect("rename inside sv after link committed");
    fs.unlink(root, b"source").expect("unlink inside sv");
    fs.rmdir(root, b"empty").expect("rmdir inside sv");
    assert_eq!(fs.subvolume_at(b"/").unwrap(), id);
    assert_eq!(fs.lookup_path("/renamed").unwrap().nlink, 1);
    assert!(fs.lookup_path("/source").is_err());
    assert!(fs.lookup_path("/empty").is_err());
    assert!(image.mount().lookup_path("/top-file").is_ok());
    let ro = Filesystem::mount(Arc::new(FileDevice::open(&image.0).unwrap())).unwrap();
    let mut ro = ro.open_subvolume(id).unwrap();
    assert!(matches!(
        ro.link(ino, root, b"refused"),
        Err(fs_btrfs::Error::ReadOnly)
    ));
}

#[test]
fn a_rename_across_a_subvolume_boundary_is_refused_without_writing() {
    let image = Scratch::new("boundary");
    let mut top = image.mount();
    let root = top.lookup_path("/").unwrap().ino;
    top.create(root, b"top-file", 0o644, 0, 0).unwrap();
    let id = top.create_subvolume(root, b"sv", 0o755, 0, 0).unwrap();
    let mut sv = top.open_subvolume(id).unwrap();
    sv.create(root, b"source", 0o644, 0, 0).unwrap();
    drop((sv, top));
    let before = sha256_hex(&std::fs::read(&image.0).unwrap());
    let c = |s: &str| CString::new(s).unwrap();
    let handle = unsafe { fs_btrfs_mount_rw(c(image.0.to_str().unwrap()).as_ptr()) };
    assert!(!handle.is_null());
    for (from, to) in [("/sv/source", "/moved"), ("/top-file", "/sv/moved")] {
        assert_eq!(
            unsafe { fs_btrfs_rename(handle, c(from).as_ptr(), c(to).as_ptr()) },
            -1,
            "{from} -> {to} crossed trees"
        );
        let error = unsafe { CStr::from_ptr(fs_btrfs_last_error()) }.to_string_lossy();
        assert!(error.contains("subvolume"), "wrong refusal: {error}");
    }
    unsafe { fs_btrfs_umount(handle) };
    assert_eq!(sha256_hex(&std::fs::read(&image.0).unwrap()), before);
}
