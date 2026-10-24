//! No-tool contract: the C ABI writes inside a subvolume other than the
//! top level through the handle `fs_btrfs_open_subvolume` returns (#520).
//! The independent on-disk validation lives in capi_subvol_write_kernel.rs.
//!
//! The volume and its subvolume are made through the library, because the
//! C ABI has no entry that creates a subvolume. Every change after that,
//! and every check of what it left, goes through the C ABI alone.

use fs_btrfs::capi::*;
use fs_btrfs::{mkfs, Filesystem};
use fs_btrfs_test_support::temp_path;
use fs_core::{BlockDevice, FileDevice};
use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::Arc;

const EROFS: i32 = 30;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

/// A scratch volume holding a file at the top and a subvolume `sv` with a
/// file in it, removed when it drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let image = PathBuf::from(temp_path!("capi-subvol-write-{tag}.img"));
        std::fs::create_dir_all(image.parent().unwrap()).unwrap();
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(128 << 20))
            .unwrap();
        let dev = FileDevice::open_rw(&image).unwrap();
        mkfs::format(&dev, &mkfs::Options::default()).unwrap();
        drop(dev);
        let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
        let mut top = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap();
        let root = top.lookup_path("/").unwrap().ino;
        top.create(root, b"top-file", 0o644, 0, 0).unwrap();
        top.create_subvolume(root, b"sv", 0o755, 0, 0).unwrap();
        Self(image)
    }

    fn c_path(&self) -> CString {
        cstr(self.0.to_str().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The id of the subvolume listed as `name` in the directory `dir`,
/// found through the C ABI's own listing.
fn subvolume_id(fs: *mut fs_btrfs_fs, dir: &str, name: &str) -> u64 {
    let d = cstr(dir);
    let iter = unsafe { fs_btrfs_dir_open(fs, d.as_ptr()) };
    assert!(!iter.is_null(), "listing {dir}: {}", last_error());
    let mut found = None;
    loop {
        let entry = unsafe { fs_btrfs_dir_next(iter) };
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        let listed = unsafe { CStr::from_ptr(entry.name.as_ptr()) };
        if listed.to_bytes() == name.as_bytes() {
            assert_ne!(entry.is_subvolume, 0, "{dir}/{name} names a subvolume");
            found = Some(entry.inode);
        }
    }
    unsafe { fs_btrfs_dir_close(iter) };
    found.unwrap_or_else(|| panic!("{dir} lists no {name}"))
}

fn names(fs: *mut fs_btrfs_fs, dir: &str) -> Vec<String> {
    let d = cstr(dir);
    let iter = unsafe { fs_btrfs_dir_open(fs, d.as_ptr()) };
    assert!(!iter.is_null(), "listing {dir}: {}", last_error());
    let mut out = Vec::new();
    loop {
        let entry = unsafe { fs_btrfs_dir_next(iter) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).name.as_ptr()) };
        let name = name.to_string_lossy().into_owned();
        if name != "." && name != ".." {
            out.push(name);
        }
    }
    unsafe { fs_btrfs_dir_close(iter) };
    out.sort();
    out
}

fn read_all(fs: *mut fs_btrfs_fs, path: &str) -> Vec<u8> {
    let c = cstr(path);
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            c.as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert!(n >= 0, "reading {path}: {}", last_error());
    buf.truncate(n as usize);
    buf
}

fn exists(fs: *mut fs_btrfs_fs, path: &str) -> bool {
    let c = cstr(path);
    let mut attr = std::mem::MaybeUninit::<fs_btrfs_attr_t>::uninit();
    unsafe { fs_btrfs_stat(fs, c.as_ptr(), attr.as_mut_ptr()) == 0 }
}

#[test]
fn namespace_changes_and_truncate_through_the_abi_stay_inside_the_subvolume() {
    let image = Scratch::new("scope");
    let top = unsafe { fs_btrfs_mount_rw(image.c_path().as_ptr()) };
    assert!(!top.is_null(), "mount rw: {}", last_error());
    let id = subvolume_id(top, "/", "sv");
    let sv = unsafe { fs_btrfs_open_subvolume(top, id) };
    assert!(!sv.is_null(), "open sv: {}", last_error());

    let made = cstr("/made");
    let ino = unsafe { fs_btrfs_create(sv, made.as_ptr(), 0o644) };
    assert_ne!(ino, 0, "create inside sv: {}", last_error());
    let payload = b"payload in sv\n";
    let wrote = unsafe {
        fs_btrfs_write_file(
            sv,
            made.as_ptr(),
            payload.as_ptr().cast::<c_void>(),
            0,
            payload.len() as u64,
        )
    };
    assert_eq!(
        wrote,
        payload.len() as i64,
        "write inside sv: {}",
        last_error()
    );
    let linked = cstr("/linked");
    let rc = unsafe { fs_btrfs_link(sv, made.as_ptr(), linked.as_ptr()) };
    assert_eq!(rc, 0, "link inside sv: {}", last_error());
    let renamed = cstr("/renamed");
    let rc = unsafe { fs_btrfs_rename(sv, linked.as_ptr(), renamed.as_ptr()) };
    assert_eq!(rc, 0, "rename inside sv: {}", last_error());
    let rc = unsafe { fs_btrfs_unlink(sv, made.as_ptr()) };
    assert_eq!(rc, 0, "unlink inside sv: {}", last_error());
    let dir = cstr("/dir");
    let dino = unsafe { fs_btrfs_mkdir(sv, dir.as_ptr(), 0o755) };
    assert_ne!(dino, 0, "mkdir inside sv: {}", last_error());
    let rc = unsafe { fs_btrfs_rmdir(sv, dir.as_ptr()) };
    assert_eq!(rc, 0, "rmdir inside sv: {}", last_error());
    let rc = unsafe { fs_btrfs_truncate(sv, renamed.as_ptr(), 7) };
    assert_eq!(rc, 0, "truncate inside sv: {}", last_error());

    assert_eq!(names(sv, "/"), ["renamed"], "sv after the changes");
    assert_eq!(read_all(sv, "/renamed"), b"payload");
    unsafe { fs_btrfs_umount(sv) };
    unsafe { fs_btrfs_umount(top) };

    let ro = unsafe { fs_btrfs_mount(image.c_path().as_ptr()) };
    assert!(!ro.is_null(), "mount ro: {}", last_error());
    assert_eq!(
        names(ro, "/"),
        ["sv", "top-file"],
        "the top level is unchanged"
    );
    assert_eq!(names(ro, "/sv"), ["renamed"]);
    assert_eq!(read_all(ro, "/sv/renamed"), b"payload");
    assert!(!exists(ro, "/renamed"), "nothing landed in the top level");
    assert!(!exists(ro, "/made"));

    let ro_sv = unsafe { fs_btrfs_open_subvolume(ro, id) };
    assert!(
        !ro_sv.is_null(),
        "open sv on a read-only mount: {}",
        last_error()
    );
    let refused = cstr("/refused");
    let ino = unsafe { fs_btrfs_create(ro_sv, refused.as_ptr(), 0o644) };
    assert_eq!(ino, 0, "a subvolume of a read-only mount refuses a create");
    assert_eq!(fs_btrfs_last_errno(), EROFS, "{}", last_error());
    unsafe { fs_btrfs_umount(ro_sv) };
    unsafe { fs_btrfs_umount(ro) };
}
