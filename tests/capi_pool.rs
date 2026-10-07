//! `fs_btrfs_mount_pool` and `fs_btrfs_open_subvolume`, the two library
//! entry points the C ABI did not export (#271).
//!
//! The pool is `btrfs-pool-a.img` and `btrfs-pool-b.img`, a two-device
//! RAID1 the kernel populated, and its manifest records the size and
//! SHA-256 of every file as the kernel read it. The subvolume fixture is
//! `btrfs-subvol`, whose manifest is `btrfs subvolume list`.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{fixture, sha256_hex};
use std::ffi::{c_char, c_void, CStr, CString};

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn read_all(fs: *mut fs_btrfs_fs, path: &str, size: usize) -> Vec<u8> {
    let c = cstr(path);
    let mut buf = vec![0u8; size + 1];
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

fn mount_pool(paths: &[std::path::PathBuf]) -> *mut fs_btrfs_fs {
    let owned: Vec<CString> = paths.iter().map(|p| cstr(p.to_str().unwrap())).collect();
    let ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    unsafe { fs_btrfs_mount_pool(ptrs.as_ptr(), ptrs.len()) }
}

/// Both devices, in either order, read every file as the kernel wrote it.
#[test]
fn a_pool_mounts_through_the_abi_and_reads_what_the_kernel_wrote() {
    let (a, b) = (fixture("btrfs-pool-a.img"), fixture("btrfs-pool-b.img"));
    let manifest = std::fs::read_to_string(fixture("btrfs-pool.manifest")).expect("manifest");
    for order in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
        let fs = mount_pool(&order);
        assert!(!fs.is_null(), "mounting the pool: {}", last_error());
        let mut checked = 0;
        for line in manifest.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 || parts[1] == "dir" {
                continue;
            }
            let size: usize = parts[1].parse().expect("a size");
            let got = read_all(fs, parts[0], size);
            assert_eq!(got.len(), size, "{}: length", parts[0]);
            assert_eq!(
                sha256_hex(&got),
                parts[2],
                "{}: not the bytes the kernel wrote",
                parts[0]
            );
            checked += 1;
        }
        assert!(checked > 0, "the manifest names no file");
        unsafe { fs_btrfs_umount(fs) };
    }
}

/// One device of the two is refused, with a message, not half read.
#[test]
fn a_pool_missing_a_device_is_refused() {
    let fs = mount_pool(&[fixture("btrfs-pool-a.img")]);
    assert!(fs.is_null(), "one device of two must not mount");
    assert!(!last_error().is_empty());
}

#[test]
fn a_null_or_empty_device_list_is_refused() {
    let fs = unsafe { fs_btrfs_mount_pool(std::ptr::null(), 2) };
    assert!(fs.is_null());
    let one = [std::ptr::null::<c_char>()];
    let fs = unsafe { fs_btrfs_mount_pool(one.as_ptr(), 0) };
    assert!(fs.is_null());
    let fs = unsafe { fs_btrfs_mount_pool(one.as_ptr(), 1) };
    assert!(fs.is_null(), "a NULL path inside the list is refused");
}

/// A subvolume opened by the id `btrfs subvolume list` gave it reads its
/// own files at paths absolute within it.
#[test]
fn a_subvolume_opens_by_its_id() {
    let manifest =
        std::fs::read_to_string(fixture("btrfs-subvol.manifest")).expect("subvol manifest");
    // `ID 256 gen .. path sub`: the id of the subvolume at path `sub`.
    let id: u64 = manifest
        .lines()
        .filter(|l| l.starts_with("ID ") && l.ends_with(" path sub"))
        .map(|l| l.split_whitespace().nth(1).unwrap().parse().unwrap())
        .next()
        .expect("btrfs subvolume list names `sub`");

    let img = cstr(fixture("btrfs-subvol.img").to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount(img.as_ptr()) };
    assert!(!fs.is_null(), "{}", last_error());
    let sub = unsafe { fs_btrfs_open_subvolume(fs, id) };
    assert!(!sub.is_null(), "opening subvolume {id}: {}", last_error());
    // Released first: the subvolume's handle does not borrow the parent.
    unsafe { fs_btrfs_umount(fs) };

    assert_eq!(read_all(sub, "/b.txt", 64), b"in sub\n");
    assert_eq!(read_all(sub, "/inner/c.txt", 64), b"in sub/inner\n");
    unsafe { fs_btrfs_umount(sub) };
}

#[test]
fn an_unknown_subvolume_id_is_enoent() {
    let img = cstr(fixture("btrfs-subvol.img").to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount(img.as_ptr()) };
    assert!(!fs.is_null(), "{}", last_error());
    let sub = unsafe { fs_btrfs_open_subvolume(fs, 999_999) };
    assert!(sub.is_null());
    assert_eq!(fs_btrfs_last_errno(), 2, "ENOENT");
    assert!(unsafe { fs_btrfs_open_subvolume(std::ptr::null_mut(), 5) }.is_null());
    unsafe { fs_btrfs_umount(fs) };
}
