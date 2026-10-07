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
