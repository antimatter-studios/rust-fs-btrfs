//! `fs_btrfs_balance`, called the way a C caller would, judged by what is
//! not this crate (#402).
//!
//! Data and metadata of a kernel-written volume are converted between
//! single and DUP and back through the call, and after each `btrfs check
//! --readonly` inspects the volume, `btrfs filesystem df` on a kernel mount
//! names the profile, and the kernel reads the file it wrote with the
//! digest it had. Each errno the call documents is tests/capi_balance.rs,
//! which needs no VM.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};

extern "C" {
    // As include/fs_btrfs.h declares it, the way a C caller reaches it: the
    // handle is opaque.
    fn fs_btrfs_balance(fs: *mut c_void, kinds: u64, convert: u64) -> i64;
}

/// `FS_BTRFS_BALANCE_SINGLE`: the kernel's `BTRFS_AVAIL_ALLOC_BIT_SINGLE`.
const FS_BTRFS_BALANCE_SINGLE: u64 = 1 << 48;

/// The chunk type bits the call takes as `kinds`.
const DATA: u64 = 1;
const METADATA: u64 = 4;
/// The profile bit it takes as `convert` for DUP.
const DUP: u64 = 1 << 5;

/// The file the kernel writes, and how its digest is read back.
const FILL: &str = "head -c 3000000 /dev/urandom > \"$MNT/data.bin\"\nsync";
const DIGEST: &str = "sha256sum \"$MNT/data.bin\" | cut -d' ' -f1";

/// A scratch volume the reference `mkfs.btrfs` made, with single data and
/// DUP metadata and a file the kernel wrote; removed when it drops, including
/// on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(512 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args([
                "-q", "-f", "-d", "single", "-m", "dup", "-s", "4096", "-n", "16384",
            ])
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
    let rc = unsafe { fs_btrfs_balance(fs.cast(), kinds, convert) };
    let (errno, message) = (fs_btrfs_last_errno(), last_error());
    unsafe { fs_btrfs_umount(fs) };
    (rc, errno, message)
}

/// The profile `btrfs filesystem df` names for `kind` (`Data`, `Metadata`),
/// on a kernel mount.
fn profile(image: &Path, kind: &str, what: &str) -> String {
    let df = guest_kernel_read_ok(
        &image.to_string_lossy(),
        what,
        "btrfs filesystem df \"$MNT\"",
    );
    let head = format!("{kind}, ");
    let found: Vec<&str> = df
        .lines()
        .filter_map(|l| l.strip_prefix(head.as_str()))
        .filter_map(|l| l.split(':').next())
        .collect();
    assert_eq!(found.len(), 1, "{kind} in more than one profile:\n{df}");
    found[0].to_string()
}

/// Balance `kinds` to `convert` through the call, and hold the volume to
/// what is not this crate: `btrfs check`, `btrfs filesystem df` naming
/// `profile_now` for `kind`, and the kernel's digest of the file.
fn convert_and_judge(
    image: &Path,
    digest: &str,
    (kinds, kind): (u64, &str),
    (convert, profile_now): (u64, &str),
) {
    let (rc, _, message) = balance(image, true, kinds, convert);
    assert!(
        rc >= 1,
        "converting {kind} to {profile_now} relocated {rc} groups: {message}"
    );
    assert_btrfs_check_clean(
        image,
        &format!("after fs_btrfs_balance converted {kind} to {profile_now}"),
    );
    assert_eq!(
        profile(image, kind, &format!("{kind} after {profile_now}")),
        profile_now
    );
    assert_eq!(
        guest_kernel_read_ok(
            &image.to_string_lossy(),
            &format!("the digest after {kind} {profile_now}"),
            DIGEST
        ),
        digest,
        "the kernel reads the file differently after {kind} went to {profile_now}"
    );
}

#[test]
fn data_and_metadata_convert_and_back_through_the_abi() {
    let scratch = Scratch::new("capi-balance-convert");
    let image = scratch.image();
    let digest = guest_kernel_read_ok(&image.to_string_lossy(), "the digest before", DIGEST);
    assert_eq!(profile(&image, "Data", "data as made"), "single");
    assert_eq!(profile(&image, "Metadata", "metadata as made"), "DUP");

    let single = FS_BTRFS_BALANCE_SINGLE;
    convert_and_judge(&image, &digest, (DATA, "Data"), (DUP, "DUP"));
    convert_and_judge(&image, &digest, (DATA, "Data"), (single, "single"));
    convert_and_judge(&image, &digest, (METADATA, "Metadata"), (single, "single"));
    convert_and_judge(&image, &digest, (METADATA, "Metadata"), (DUP, "DUP"));
}
