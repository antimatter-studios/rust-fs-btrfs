//! `fs_btrfs_balance` (#402): what the C ABI answers, on this crate's own
//! mkfs volume.
//!
//! A C caller has the return value, the thread's message and the errno, so
//! every error the header documents is provoked here and its errno checked,
//! with the image required to be byte-for-byte what it was: EROFS on a
//! read-only handle, EINVAL for kinds and conversions the call cannot mean,
//! ENOTSUP for what relocation refuses. What the kernel and `btrfs check`
//! make of a balance through the call is tests/capi_balance_kernel.rs.
//!
//! No fixture, no VM: the unit tier.

use fs_btrfs::balance::profile_bits;
use fs_btrfs::capi::*;
use fs_btrfs::chunk::block_group as bg;
use fs_btrfs::{mkfs, ChunkProfile, Filesystem};
use fs_btrfs_test_support::temp_path;
use fs_core::{BlockDevice, FileDevice};
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

extern "C" {
    // As include/fs_btrfs.h declares it, the way a C caller reaches it: the
    // handle is opaque.
    fn fs_btrfs_balance(fs: *mut c_void, kinds: u64, convert: u64) -> i64;
}

/// `FS_BTRFS_BALANCE_SINGLE`: the kernel's `BTRFS_AVAIL_ALLOC_BIT_SINGLE`.
const FS_BTRFS_BALANCE_SINGLE: u64 = 1 << 48;

const EINVAL: i32 = 22;
const EIO: i32 = 5;
const EROFS: i32 = 30;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

/// What the file holds, in sectors that are not all alike.
fn payload() -> Vec<u8> {
    (0..300_000u32).map(|i| (i % 251) as u8).collect()
}

/// A fresh volume made by this crate's mkfs, holding one file; removed
/// when it drops, including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let dev = FileDevice::open_rw(&image).unwrap();
        mkfs::format(&dev, &mkfs::Options::default()).unwrap();
        drop(dev);
        let mut fs = mount_rw(&image);
        let root = fs.lookup_path("/").unwrap().ino;
        let file = fs.create(root, b"file", 0o644, 0, 0).unwrap();
        fs.write(file, 0, &payload()).unwrap();
        Scratch(dir)
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).unwrap());
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

/// Mount `image`, read-write or not, call `fs_btrfs_balance` and unmount:
/// what it returned, and the errno and message it left.
fn balance(image: &Path, writable: bool, kinds: u64, convert: u64) -> (i64, i32, String) {
    let device = CString::new(image.to_str().unwrap()).unwrap();
    let fs = if writable {
        unsafe { fs_btrfs_mount_rw(device.as_ptr()) }
    } else {
        unsafe { fs_btrfs_mount(device.as_ptr()) }
    };
    assert!(!fs.is_null(), "mount: {}", last_error());
    let rc = unsafe { fs_btrfs_balance(fs.cast(), kinds, convert) };
    let (errno, message) = (fs_btrfs_last_errno(), last_error());
    unsafe { fs_btrfs_umount(fs) };
    (rc, errno, message)
}

/// The profile of every block group holding `kind`.
fn profiles(image: &Path, kind: u64) -> Vec<ChunkProfile> {
    mount_rw(image)
        .block_groups()
        .unwrap()
        .iter()
        .filter(|g| g.flags & kind != 0)
        .map(|g| ChunkProfile::from_type(g.flags).unwrap())
        .collect()
}

/// The file, read back through the library.
fn file_bytes(image: &Path) -> Vec<u8> {
    let fs = mount_rw(image);
    let ino = fs.lookup_path("/file").unwrap().ino;
    fs.read_file(ino).unwrap()
}

#[test]
fn every_documented_error_has_its_errno_and_writes_nothing() {
    let scratch = Scratch::new("capi-balance-errors");
    let image = scratch.image();
    let before = std::fs::read(&image).unwrap();

    let (dup, raid1) = (
        profile_bits(ChunkProfile::Dup),
        profile_bits(ChunkProfile::Raid1),
    );
    let cases: [(&str, bool, u64, u64, i32); 12] = [
        ("a read-only handle", false, bg::DATA, 0, EROFS),
        ("no kinds", true, 0, 0, EINVAL),
        ("a kind that is not one", true, 8, 0, EINVAL),
        (
            "a kind beside one that is not",
            true,
            bg::DATA | 8,
            0,
            EINVAL,
        ),
        ("two profiles at once", true, bg::DATA, dup | raid1, EINVAL),
        (
            "single beside a profile",
            true,
            bg::DATA,
            FS_BTRFS_BALANCE_SINGLE | dup,
            EINVAL,
        ),
        (
            "a bit that is no profile",
            true,
            bg::DATA,
            bg::METADATA,
            EINVAL,
        ),
        ("a bit past every profile", true, bg::DATA, 1 << 49, EINVAL),
        ("a system group", true, bg::SYSTEM, 0, ENOTSUP),
        // The data groups alone could move: the check comes before any does.
        (
            "data beside a system group",
            true,
            bg::DATA | bg::SYSTEM,
            0,
            ENOTSUP,
        ),
        ("raid1 on one device", true, bg::DATA, raid1, ENOTSUP),
        (
            "raid1 for metadata too",
            true,
            bg::DATA | bg::METADATA,
            raid1,
            ENOTSUP,
        ),
    ];
    for (what, writable, kinds, convert, errno) in cases {
        let (rc, got, message) = balance(&image, writable, kinds, convert);
        assert_eq!(rc, -1, "{what}: the balance was not refused");
        assert_eq!(got, errno, "{what}: errno {got}, {message}");
        assert!(
            std::fs::read(&image).unwrap() == before,
            "{what}: the refused balance changed the image"
        );
    }
}

#[test]
fn a_null_handle_is_an_error_rather_than_a_crash() {
    let rc = unsafe { fs_btrfs_balance(std::ptr::null_mut(), bg::DATA, 0) };
    assert_eq!(rc, -1);
    assert_eq!(fs_btrfs_last_errno(), EIO, "{}", last_error());
}

#[test]
fn a_group_already_in_the_profile_stays_and_nothing_is_written() {
    let scratch = Scratch::new("capi-balance-soft");
    let image = scratch.image();
    assert!(profiles(&image, bg::DATA)
        .iter()
        .all(|p| *p == ChunkProfile::Single));
    let before = std::fs::read(&image).unwrap();
    let (rc, _, message) = balance(&image, true, bg::DATA, FS_BTRFS_BALANCE_SINGLE);
    assert_eq!(rc, 0, "{message}");
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a balance that relocated nothing changed the image"
    );
}

#[test]
fn data_is_relocated_and_converted_there_and_back_with_a_count_of_the_groups() {
    let scratch = Scratch::new("capi-balance-convert");
    let image = scratch.image();
    let groups = profiles(&image, bg::DATA).len() as i64;
    let starts = |image: &Path| -> Vec<u64> {
        mount_rw(image)
            .block_groups()
            .unwrap()
            .iter()
            .filter(|g| g.flags & bg::DATA != 0)
            .map(|g| g.start)
            .collect()
    };

    let was = starts(&image);
    let (rc, _, message) = balance(&image, true, bg::DATA, 0);
    assert_eq!(rc, groups, "{message}");
    assert!(
        starts(&image).iter().all(|at| !was.contains(at)),
        "a data group stayed where it was"
    );
    assert_eq!(file_bytes(&image), payload());

    let dup = profile_bits(ChunkProfile::Dup);
    let (rc, _, message) = balance(&image, true, bg::DATA, dup);
    assert_eq!(rc, groups, "{message}");
    assert!(profiles(&image, bg::DATA)
        .iter()
        .all(|p| *p == ChunkProfile::Dup));
    assert_eq!(file_bytes(&image), payload());

    let (rc, _, message) = balance(&image, true, bg::DATA, FS_BTRFS_BALANCE_SINGLE);
    assert_eq!(rc, groups, "{message}");
    assert!(profiles(&image, bg::DATA)
        .iter()
        .all(|p| *p == ChunkProfile::Single));
    assert_eq!(file_bytes(&image), payload());
}
