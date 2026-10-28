//! `fs_btrfs_balance`, called the way a C caller would, judged by what is
//! not this crate (#402).
//!
//! A C caller has the return value, the thread's message and the errno,
//! so every error the header documents is provoked here and its errno
//! checked, with the image required to be byte-for-byte what it was:
//! EROFS on a read-only handle, EINVAL for kinds and conversions the call
//! cannot mean, ENOTSUP for what relocation refuses. Then the data of a
//! kernel-written volume is converted to DUP and back through the call,
//! and after each `btrfs check --readonly` inspects the volume, `btrfs
//! filesystem df` on a kernel mount names the profile, and the kernel
//! reads the file it wrote with the digest it had.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

const EINVAL: i32 = 22;
const EROFS: i32 = 30;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

/// The chunk type bits the call takes as `kinds`.
const DATA: u64 = 1;
const SYSTEM: u64 = 2;
const METADATA: u64 = 4;
/// The profile bits it takes as `convert`.
const RAID1: u64 = 1 << 4;
const DUP: u64 = 1 << 5;

/// The file the kernel writes, and how its digest is read back.
const FILL: &str = "head -c 3000000 /dev/urandom > \"$MNT/data.bin\"\nsync";
const DIGEST: &str = "sha256sum \"$MNT/data.bin\" | cut -d' ' -f1";

/// A scratch volume the reference `mkfs.btrfs` made, with single data the
/// kernel wrote a file into; removed when it drops, including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-d", "single", "-s", "4096", "-n", "16384"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(&image.to_string_lossy(), name, FILL);
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
    let rc = unsafe { fs_btrfs_balance(fs, kinds, convert) };
    let (errno, message) = (fs_btrfs_last_errno(), last_error());
    unsafe { fs_btrfs_umount(fs) };
    (rc, errno, message)
}

/// The profile `btrfs filesystem df` names for the data, on a kernel
/// mount.
fn data_profile(image: &Path, what: &str) -> String {
    let df = guest_kernel_read_ok(
        &image.to_string_lossy(),
        what,
        "btrfs filesystem df \"$MNT\"",
    );
    let data: Vec<&str> = df
        .lines()
        .filter_map(|l| l.strip_prefix("Data, "))
        .filter_map(|l| l.split(':').next())
        .collect();
    assert_eq!(data.len(), 1, "data in more than one profile:\n{df}");
    data[0].to_string()
}

#[test]
fn every_documented_error_has_its_errno_and_writes_nothing() {
    let scratch = Scratch::new("capi-balance-errors");
    let image = scratch.image();
    let before = std::fs::read(&image).unwrap();

    let cases: [(&str, bool, u64, u64, i32); 8] = [
        ("a read-only handle", false, DATA, 0, EROFS),
        ("no kinds", true, 0, 0, EINVAL),
        ("a kind that is not one", true, 8, 0, EINVAL),
        ("two profiles at once", true, DATA, DUP | RAID1, EINVAL),
        ("a bit that is not a profile", true, DATA, METADATA, EINVAL),
        ("a bit past every profile", true, DATA, 1 << 49, EINVAL),
        ("a system group", true, SYSTEM, 0, ENOTSUP),
        ("raid1 on one device", true, DATA, RAID1, ENOTSUP),
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

    // Single data asked to be single: nothing to relocate, nothing written.
    let (rc, _, message) = balance(&image, true, DATA, FS_BTRFS_BALANCE_SINGLE);
    assert_eq!(rc, 0, "{message}");
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a balance that relocated nothing changed the image"
    );
}

#[test]
fn data_converts_to_dup_and_back_through_the_abi() {
    let scratch = Scratch::new("capi-balance-convert");
    let image = scratch.image();
    let digest = guest_kernel_read_ok(&image.to_string_lossy(), "the digest before", DIGEST);
    assert_eq!(
        data_profile(&image, "the profile mkfs.btrfs made"),
        "single"
    );

    let (rc, _, message) = balance(&image, true, DATA, DUP);
    assert!(
        rc >= 1,
        "converting to DUP relocated {rc} groups: {message}"
    );
    assert_btrfs_check_clean(&image, "after fs_btrfs_balance converted the data to DUP");
    assert_eq!(
        data_profile(&image, "the profile after converting to DUP"),
        "DUP"
    );
    assert_eq!(
        guest_kernel_read_ok(&image.to_string_lossy(), "the digest after DUP", DIGEST),
        digest,
        "the kernel reads the file differently after the conversion to DUP"
    );

    let (rc, _, message) = balance(&image, true, DATA, FS_BTRFS_BALANCE_SINGLE);
    assert!(
        rc >= 1,
        "converting to single relocated {rc} groups: {message}"
    );
    assert_btrfs_check_clean(&image, "after fs_btrfs_balance converted the data back");
    assert_eq!(
        data_profile(&image, "the profile after converting back"),
        "single"
    );
    assert_eq!(
        guest_kernel_read_ok(&image.to_string_lossy(), "the digest after single", DIGEST),
        digest,
        "the kernel reads the file differently after the conversion back to single"
    );
}
