//! The C ABI's path entry points cross into subvolumes, as a mount does
//! (#271).
//!
//! `fs_btrfs_dir_open` always crossed; `fs_btrfs_stat`,
//! `fs_btrfs_read_file`, `fs_btrfs_readlink` and the attribute calls
//! stopped at the boundary, so a caller could list a subvolume and then
//! read none of what it listed. The fixture is `btrfs-subvol`, made by the
//! kernel; its manifest records what the kernel read at each path.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::fixture;
use std::ffi::{c_void, CStr, CString};

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn mount() -> *mut fs_btrfs_fs {
    let path = fixture("btrfs-subvol.img");
    let c = cstr(path.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount(c.as_ptr()) };
    assert!(!fs.is_null(), "mounting btrfs-subvol: {}", last_error());
    fs
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

/// Every file the kernel wrote inside a subvolume stats and reads through
/// the ABI by its path from the top of the filesystem.
#[test]
fn stat_and_read_cross_into_subvolumes() {
    let fs = mount();
    for (path, want) in [
        ("/sub/b.txt", "in sub\n"),
        ("/sub/inner/c.txt", "in sub/inner\n"),
        ("/snap/b.txt", "in sub\n"),
        ("/rosnap/b.txt", "in sub\n"),
        ("/sub/inner/../b.txt", "in sub\n"),
    ] {
        let c = cstr(path);
        let mut attr: fs_btrfs_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_btrfs_stat(fs, c.as_ptr(), &mut attr) };
        assert_eq!(rc, 0, "stat {path}: {}", last_error());
        assert_eq!(attr.size, want.len() as u64, "{path}: size");
        assert_eq!(read_all(fs, path), want.as_bytes(), "{path}: contents");
    }
    unsafe { fs_btrfs_umount(fs) };
}

/// A subvolume's own top directory stats as a directory, not as the
/// entry's objectid read as an inode of the parent tree.
#[test]
fn a_subvolume_top_stats_as_a_directory() {
    let fs = mount();
    let c = cstr("/sub/inner");
    let mut attr: fs_btrfs_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_btrfs_stat(fs, c.as_ptr(), &mut attr) };
    assert_eq!(rc, 0, "stat /sub/inner: {}", last_error());
    assert_eq!(attr.mode & 0o170000, 0o040000, "a directory");
    // BTRFS_FIRST_FREE_OBJECTID: every subvolume's top directory.
    assert_eq!(attr.inode, 256);
    unsafe { fs_btrfs_umount(fs) };
}

/// A scratch copy of the fixture, for the calls that write (#267).
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "capi-subvol-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("btrfs-subvol.img");
    std::fs::copy(fixture("btrfs-subvol.img"), &image).expect("copy the fixture");
    image
}

fn stat(fs: *mut fs_btrfs_fs, path: &str) -> fs_btrfs_attr_t {
    let c = cstr(path);
    let mut attr: fs_btrfs_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_btrfs_stat(fs, c.as_ptr(), &mut attr) };
    assert_eq!(rc, 0, "stat {path}: {}", last_error());
    attr
}

/// `fs_btrfs_subvol_create` returns the new subvolume's id, the next
/// after the highest the fixture holds, and the path then stats as that
/// subvolume's top directory with the requested mode, owned like the
/// directory it was made in.
#[test]
fn subvol_create_returns_the_new_subvolume_id() {
    let image = scratch("create");
    let c_img = cstr(image.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount_rw(c_img.as_ptr()) };
    assert!(!fs.is_null(), "mount rw: {}", last_error());
    let parent = stat(fs, "/top");
    let path = cstr("/top/made");
    let id = unsafe { fs_btrfs_subvol_create(fs, path.as_ptr(), 0o750) };
    // The fixture's highest subvolume is 260 (top/nested).
    assert_eq!(id, 261, "subvol_create: {}", last_error());
    assert_eq!(fs_btrfs_last_errno(), 0);
    let made = stat(fs, "/top/made");
    assert_eq!(made.inode, 256, "a subvolume's top directory");
    assert_eq!(made.mode & 0o170000, 0o040000, "a directory");
    assert_eq!(made.mode & 0o7777, 0o750, "the requested mode");
    assert_eq!((made.uid, made.gid), (parent.uid, parent.gid), "owner");
    unsafe { fs_btrfs_umount(fs) };

    // Mounted again, the listing names it a subvolume with that id.
    let fs = unsafe { fs_btrfs_mount(c_img.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_error());
    let top = cstr("/top");
    let iter = unsafe { fs_btrfs_dir_open(fs, top.as_ptr()) };
    assert!(!iter.is_null(), "listing /top: {}", last_error());
    let mut found = None;
    loop {
        let entry = unsafe { fs_btrfs_dir_next(iter) };
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        let name = unsafe { CStr::from_ptr(entry.name.as_ptr()) };
        if name.to_bytes() == b"made" {
            found = Some((entry.inode, entry.is_subvolume));
        }
    }
    unsafe { fs_btrfs_dir_close(iter) };
    assert_eq!(found, Some((261, 1)), "/top lists made as subvolume 261");
    unsafe { fs_btrfs_umount(fs) };
    let _ = std::fs::remove_dir_all(image.parent().unwrap());
}

/// Every refusal returns 0 with its errno and leaves the image byte for
/// byte as it was.
#[test]
fn subvol_create_refusals_change_nothing() {
    let image = scratch("refuse");
    let before = std::fs::read(&image).expect("read the image");
    let c_img = cstr(image.to_str().unwrap());

    let path = cstr("/made");
    assert_eq!(
        unsafe { fs_btrfs_subvol_create(std::ptr::null_mut(), path.as_ptr(), 0o755) },
        0
    );
    assert_eq!(fs_btrfs_last_errno(), libc_errno::EIO, "NULL fs");

    let ro = unsafe { fs_btrfs_mount(c_img.as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    assert_eq!(
        unsafe { fs_btrfs_subvol_create(ro, path.as_ptr(), 0o755) },
        0
    );
    assert_eq!(fs_btrfs_last_errno(), libc_errno::EROFS, "read-only handle");
    unsafe { fs_btrfs_umount(ro) };

    let fs = unsafe { fs_btrfs_mount_rw(c_img.as_ptr()) };
    assert!(!fs.is_null(), "mount rw: {}", last_error());
    assert_eq!(
        unsafe { fs_btrfs_subvol_create(fs, std::ptr::null(), 0o755) },
        0
    );
    assert_eq!(fs_btrfs_last_errno(), libc_errno::ENOENT, "NULL path");
    for (what, path, errno) in [
        ("the top itself", "/", libc_errno::EINVAL),
        ("a name taken by a file", "/top/a.txt", libc_errno::EEXIST),
        ("a name taken by a subvolume", "/sub", libc_errno::EEXIST),
        ("a missing directory", "/missing/made", libc_errno::ENOENT),
        (
            "a file as the directory",
            "/top/a.txt/made",
            libc_errno::ENOTDIR,
        ),
        ("a name no directory can hold", "/..", libc_errno::EINVAL),
    ] {
        let c = cstr(path);
        assert_eq!(
            unsafe { fs_btrfs_subvol_create(fs, c.as_ptr(), 0o755) },
            0,
            "{what}"
        );
        assert_eq!(fs_btrfs_last_errno(), errno, "{what}: {}", last_error());
    }
    // Inside another subvolume: the path stops at the boundary, so the
    // new subvolume can never land in the top level's directory of the
    // same inode number.
    let inside = cstr("/sub/made");
    assert_eq!(
        unsafe { fs_btrfs_subvol_create(fs, inside.as_ptr(), 0o755) },
        0
    );
    assert_eq!(
        fs_btrfs_last_errno(),
        libc_errno::ENOTSUP,
        "{}",
        last_error()
    );
    // A handle on a subvolume other than the top level writes none.
    let sub = unsafe { fs_btrfs_open_subvolume(fs, 256) };
    assert!(!sub.is_null(), "open sub: {}", last_error());
    assert_eq!(
        unsafe { fs_btrfs_subvol_create(sub, path.as_ptr(), 0o755) },
        0
    );
    assert_ne!(fs_btrfs_last_errno(), 0, "a subvolume handle");
    unsafe { fs_btrfs_umount(sub) };
    unsafe { fs_btrfs_umount(fs) };

    let after = std::fs::read(&image).expect("read the image");
    assert!(before == after, "a refused create changed the image");
    let _ = std::fs::remove_dir_all(image.parent().unwrap());
}

/// A fresh 128 MiB volume made by this crate's mkfs, for the deletes that
/// need subvolumes of their own to remove: no fixture, no VM.
fn fresh(tag: &str) -> std::path::PathBuf {
    use fs_core::FileDevice;
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "capi-subvol-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("fresh.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(128 << 20))
        .expect("size the image");
    let dev = FileDevice::open_rw(&image).expect("open the image");
    fs_btrfs::mkfs::format(&dev, &fs_btrfs::mkfs::Options::default()).expect("mkfs");
    image
}

fn mount_rw(image: &std::path::Path) -> *mut fs_btrfs_fs {
    let c = cstr(image.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount_rw(c.as_ptr()) };
    assert!(!fs.is_null(), "mount rw: {}", last_error());
    fs
}

fn subvol_create(fs: *mut fs_btrfs_fs, path: &str) -> u64 {
    let c = cstr(path);
    let id = unsafe { fs_btrfs_subvol_create(fs, c.as_ptr(), 0o755) };
    assert_ne!(id, 0, "subvol_create {path}: {}", last_error());
    id
}

fn subvol_delete(fs: *mut fs_btrfs_fs, path: &str) -> i32 {
    let c = cstr(path);
    unsafe { fs_btrfs_subvol_delete(fs, c.as_ptr()) }
}

/// Every name `path` lists, with whether it is a subvolume.
fn listing(fs: *mut fs_btrfs_fs, path: &str) -> Vec<(String, u8)> {
    let c = cstr(path);
    let iter = unsafe { fs_btrfs_dir_open(fs, c.as_ptr()) };
    assert!(!iter.is_null(), "listing {path}: {}", last_error());
    let mut names = Vec::new();
    loop {
        let entry = unsafe { fs_btrfs_dir_next(iter) };
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        let name = unsafe { CStr::from_ptr(entry.name.as_ptr()) };
        names.push((name.to_string_lossy().into_owned(), entry.is_subvolume));
    }
    unsafe { fs_btrfs_dir_close(iter) };
    names.sort();
    names
}

/// `fs_btrfs_subvol_delete` removes a subvolume -- a read-only one too,
/// and one in a directory below the top -- and after a remount its path
/// no longer resolves and its directory no longer lists it.
#[test]
fn subvol_delete_removes_the_subvolume() {
    let image = fresh("delete");
    let fs = mount_rw(&image);
    subvol_create(fs, "/keep");
    subvol_create(fs, "/gone");
    subvol_create(fs, "/ro");
    let ro = cstr("/ro");
    assert_eq!(
        unsafe { fs_btrfs_subvol_set_readonly(fs, ro.as_ptr(), 1) },
        0,
        "set /ro read-only: {}",
        last_error()
    );
    let dir = cstr("/dir");
    assert_ne!(
        unsafe { fs_btrfs_mkdir(fs, dir.as_ptr(), 0o755) },
        0,
        "mkdir /dir: {}",
        last_error()
    );
    subvol_create(fs, "/dir/inner");

    for path in ["/gone", "/ro", "/dir/inner"] {
        assert_eq!(
            subvol_delete(fs, path),
            0,
            "delete {path}: {}",
            last_error()
        );
        assert_eq!(fs_btrfs_last_errno(), 0, "delete {path}");
    }
    let gone = cstr("/gone");
    let mut attr: fs_btrfs_attr_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { fs_btrfs_stat(fs, gone.as_ptr(), &mut attr) }, -1);
    assert_eq!(fs_btrfs_last_errno(), libc_errno::ENOENT, "/gone resolves");
    unsafe { fs_btrfs_umount(fs) };

    let c_img = cstr(image.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount(c_img.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_error());
    assert_eq!(
        listing(fs, "/"),
        vec![("dir".to_string(), 0), ("keep".to_string(), 1)],
        "/ after the deletes"
    );
    assert_eq!(listing(fs, "/dir"), vec![], "/dir after the delete");
    for path in ["/gone", "/ro", "/dir/inner"] {
        let c = cstr(path);
        let mut attr: fs_btrfs_attr_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { fs_btrfs_stat(fs, c.as_ptr(), &mut attr) },
            -1,
            "{path} still resolves"
        );
        assert_eq!(fs_btrfs_last_errno(), libc_errno::ENOENT, "{path}");
        assert_eq!(
            unsafe { fs_btrfs_subvol_is_readonly(fs, c.as_ptr()) },
            -1,
            "{path} is still a subvolume"
        );
    }
    let keep = cstr("/keep");
    assert_eq!(unsafe { fs_btrfs_subvol_is_readonly(fs, keep.as_ptr()) }, 0);
    unsafe { fs_btrfs_umount(fs) };
    let _ = std::fs::remove_dir_all(image.parent().unwrap());
}

/// Every refusal returns -1 with its errno and leaves the image byte for
/// byte as it was: NULL arguments, a read-only handle, the default
/// subvolume, a name that is missing or is no subvolume, and a handle on
/// another subvolume.
#[test]
fn subvol_delete_refusals_change_nothing() {
    let image = fresh("delete-refuse");
    let fs = mount_rw(&image);
    subvol_create(fs, "/kept");
    let default = subvol_create(fs, "/default");
    let c_default = cstr("/default");
    assert_eq!(
        unsafe { fs_btrfs_subvol_set_default(fs, c_default.as_ptr()) },
        0,
        "set-default: {}",
        last_error()
    );
    let plain = cstr("/plain");
    assert_ne!(unsafe { fs_btrfs_mkdir(fs, plain.as_ptr(), 0o755) }, 0);
    let file = cstr("/file");
    assert_ne!(unsafe { fs_btrfs_create(fs, file.as_ptr(), 0o644) }, 0);
    unsafe { fs_btrfs_umount(fs) };
    let before = std::fs::read(&image).expect("read the image");
    let c_img = cstr(image.to_str().unwrap());

    let kept = cstr("/kept");
    assert_eq!(
        unsafe { fs_btrfs_subvol_delete(std::ptr::null_mut(), kept.as_ptr()) },
        -1
    );
    assert_eq!(fs_btrfs_last_errno(), libc_errno::EIO, "NULL fs");

    let ro = unsafe { fs_btrfs_mount(c_img.as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    assert_eq!(subvol_delete(ro, "/kept"), -1);
    assert_eq!(fs_btrfs_last_errno(), libc_errno::EROFS, "read-only handle");
    unsafe { fs_btrfs_umount(ro) };

    let fs = mount_rw(&image);
    assert_eq!(unsafe { fs_btrfs_subvol_delete(fs, std::ptr::null()) }, -1);
    assert_eq!(fs_btrfs_last_errno(), libc_errno::ENOENT, "NULL path");
    for (what, path, errno) in [
        ("the default subvolume", "/default", libc_errno::ENOTSUP),
        ("a missing name", "/missing", libc_errno::ENOENT),
        ("an ordinary directory", "/plain", libc_errno::ENOENT),
        ("a file", "/file", libc_errno::ENOENT),
        ("a missing directory", "/missing/kept", libc_errno::ENOENT),
        ("a file as the directory", "/file/kept", libc_errno::ENOTDIR),
        ("the top itself", "/", libc_errno::EINVAL),
    ] {
        assert_eq!(subvol_delete(fs, path), -1, "{what}");
        assert_eq!(fs_btrfs_last_errno(), errno, "{what}: {}", last_error());
    }
    // A handle on a subvolume other than the top level deletes nothing.
    let sub = unsafe { fs_btrfs_open_subvolume(fs, default) };
    assert!(!sub.is_null(), "open default: {}", last_error());
    assert_eq!(subvol_delete(sub, "/kept"), -1, "a subvolume handle");
    assert_ne!(fs_btrfs_last_errno(), 0, "a subvolume handle");
    unsafe { fs_btrfs_umount(sub) };
    unsafe { fs_btrfs_umount(fs) };

    let after = std::fs::read(&image).expect("read the image");
    assert!(before == after, "a refused delete changed the image");
    let _ = std::fs::remove_dir_all(image.parent().unwrap());
}

/// A subvolume holding another is refused with ENOTSUP, as the kernel
/// refuses it with ENOTEMPTY, and the kernel-made image is unchanged:
/// `/sub` holds `/sub/inner`.
#[test]
fn subvol_delete_refuses_a_subvolume_holding_another() {
    let image = scratch("delete-held");
    let before = std::fs::read(&image).expect("read the image");
    let fs = mount_rw(&image);
    assert_eq!(subvol_delete(fs, "/sub"), -1);
    assert_eq!(
        fs_btrfs_last_errno(),
        libc_errno::ENOTSUP,
        "{}",
        last_error()
    );
    assert!(
        last_error().contains("holds other subvolumes"),
        "{}",
        last_error()
    );
    unsafe { fs_btrfs_umount(fs) };
    let after = std::fs::read(&image).expect("read the image");
    assert!(before == after, "a refused delete changed the image");
    let _ = std::fs::remove_dir_all(image.parent().unwrap());
}

/// The errno values the header documents, as the platform numbers them.
mod libc_errno {
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const ENOTDIR: i32 = 20;
    pub const EINVAL: i32 = 22;
    pub const EEXIST: i32 = 17;
    pub const EROFS: i32 = 30;
    #[cfg(target_os = "macos")]
    pub const ENOTSUP: i32 = 45;
    #[cfg(not(target_os = "macos"))]
    pub const ENOTSUP: i32 = 95;
}
