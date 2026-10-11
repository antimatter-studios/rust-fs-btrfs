//! Filesystems grown and shrunk through the C ABI, judged by what is not
//! this crate (#264).
//!
//! A resize edits the device item in the chunk tree, the superblock's
//! size and the superblock's copy of the device item, in one committed
//! transaction that moves the chunk tree's root. `btrfs check` inspects
//! the whole volume — a device item that disagrees with the superblock, a
//! chunk tree block outside a SYSTEM group, or extent records that do not
//! match the moved blocks are each reported there — and `dump-super`
//! reads the two sizes back. Then the kernel mounts the volume, reports
//! the size it believes, and, for a grown one, fills it past the old end,
//! which it can only do by allocating in the new space; `btrfs check`
//! looks again after that.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_super, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

const MIB: u64 = 1 << 20;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

/// A scratch volume of `size` bytes, removed when it drops — including on
/// a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str, size: u64) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(size))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "16384"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(
            &image.to_string_lossy(),
            name,
            "head -c 100000 /dev/urandom > \"$MNT/before.bin\"\nsync",
        );
        Scratch(dir)
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }

    fn set_len(&self, size: u64) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(self.image())
            .and_then(|f| f.set_len(size))
            .unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Resize the image at `image` through the ABI, returning its status.
fn resize(image: &Path, size: u64) -> i32 {
    let device = CString::new(image.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_btrfs_mount_rw(device.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    let rc = unsafe { fs_btrfs_resize(fs, size) };
    unsafe { fs_btrfs_umount(fs) };
    rc
}

/// The superblock's size and its device item's, from `dump-super`.
fn sizes(image: &Path) -> (String, String) {
    let text = dump_super(image);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| {
                let mut words = l.split_whitespace();
                (words.next() == Some(name)).then(|| words.next().unwrap_or("").to_string())
            })
            .unwrap_or_else(|| panic!("dump-super has no {name}:\n{text}"))
    };
    (field("total_bytes"), field("dev_item.total_bytes"))
}

/// What the kernel says the device's size is.
fn kernel_size(image: &Path, what: &str) -> String {
    guest_kernel_read_ok(
        &image.to_string_lossy(),
        what,
        "btrfs filesystem usage -b \"$MNT\" | awk '/Device size:/ {print $3}'",
    )
    .trim()
    .to_string()
}

#[test]
fn a_filesystem_grown_through_the_abi_is_clean_and_the_kernel_fills_it() {
    let scratch = Scratch::new("resize-grow", 256 * MIB);
    let image = scratch.image();

    // Past the device: refused, and nothing written.
    let before = std::fs::read(&image).unwrap();
    assert_eq!(
        resize(&image, 512 * MIB),
        -1,
        "growing past the device succeeded"
    );
    assert_eq!(fs_btrfs_last_errno(), ENOTSUP, "{}", last_error());
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused resize wrote to the image"
    );

    scratch.set_len(512 * MIB);
    assert_eq!(resize(&image, 512 * MIB), 0, "{}", last_error());

    assert_btrfs_check_clean(&image, "after growing through the ABI");
    let want = (512 * MIB).to_string();
    assert_eq!(sizes(&image), (want.clone(), want.clone()));
    assert_eq!(kernel_size(&image, "after growing"), want);

    // 300 MiB of data does not fit in the old 256 MiB: the kernel must
    // allocate chunks in the space the resize added.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel fills the grown volume",
        "dd if=/dev/zero of=\"$MNT/big\" bs=1M count=300 status=none\nsync",
    );
    assert_btrfs_check_clean(&image, "after the kernel filled the grown volume");
}

#[test]
fn a_filesystem_shrunk_through_the_abi_is_clean_and_the_kernel_mounts_it() {
    let scratch = Scratch::new("resize-shrink", 512 * MIB);
    let image = scratch.image();

    // Past the chunks mkfs placed: refused, and nothing written.
    let before = std::fs::read(&image).unwrap();
    assert_eq!(
        resize(&image, 32 * MIB),
        -1,
        "shrinking past a chunk succeeded"
    );
    assert_eq!(fs_btrfs_last_errno(), ENOTSUP, "{}", last_error());
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused resize wrote to the image"
    );

    assert_eq!(resize(&image, 448 * MIB), 0, "{}", last_error());
    scratch.set_len(448 * MIB);

    assert_btrfs_check_clean(&image, "after shrinking through the ABI");
    let want = (448 * MIB).to_string();
    assert_eq!(sizes(&image), (want.clone(), want.clone()));
    assert_eq!(kernel_size(&image, "after shrinking"), want);
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel writes to the shrunk volume",
        "head -c 1000000 /dev/urandom > \"$MNT/after.bin\"\nsync",
    );
    assert_btrfs_check_clean(&image, "after the kernel wrote to the shrunk volume");
}
