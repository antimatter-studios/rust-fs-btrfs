//! Files truncated through the C ABI, judged by what is not this crate
//! (#262).
//!
//! A kernel-made volume holds a checksummed file of three extents, a
//! single-extent file, an inline file, an empty file and a reflinked pair.
//! Through the ABI one file is cut inside its second extent (the third is
//! released, the second referenced less), one is cut to nothing (its
//! extent released), the inline one is cut short and the empty one grown —
//! each call one committed transaction. Then `btrfs check` inspects the
//! whole volume: an extent still recorded but no longer referenced, a
//! free-space tree or block group `used` that disagrees, digests left for
//! a released extent, or an inode whose `nbytes` does not match its items
//! are each reported there. `btrfs check --check-data-csum` compares every
//! data sector left with its digest, the kernel mounts the volume and
//! reports each file's size and SHA-256, and a kernel scrub verifies every
//! sector. Last, the kernel writes to and truncates the same files and
//! `btrfs check` looks again.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, sha256_hex,
    temp_path,
};
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

const ENOENT: i32 = 2;
const EISDIR: i32 = 21;
const EROFS: i32 = 30;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

/// A scratch volume, removed when it drops — including on a panic.
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
            .args(["-q", "-f", "-s", "4096", "-n", "16384", "-O", "no-holes"])
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
            "cd \"$MNT\"\n\
             head -c 65536 /dev/urandom > multi\n\
             sync\n\
             head -c 65536 /dev/urandom >> multi\n\
             sync\n\
             head -c 65536 /dev/urandom >> multi\n\
             sync\n\
             head -c 300000 /dev/urandom > big\n\
             printf 'hello, inline world' > small\n\
             touch empty\n\
             head -c 8192 /dev/urandom > shared\n\
             cp --reflink=always shared shared2\n\
             mkdir dir\n\
             sync",
        );
        Scratch(dir)
    }

    fn image(&self) -> String {
        self.0.join("fs.img").to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn truncate(fs: *mut fs_btrfs_fs, path: &str, size: u64) -> i32 {
    unsafe { fs_btrfs_truncate(fs, c(path).as_ptr(), size) }
}

#[track_caller]
fn refused(fs: *mut fs_btrfs_fs, path: &str, size: u64, want: i32) {
    assert_eq!(
        truncate(fs, path, size),
        -1,
        "truncate {path} {size} succeeded and should not have"
    );
    assert_eq!(
        fs_btrfs_last_errno(),
        want,
        "truncate {path} {size}: wrong errno for {:?}",
        last_error()
    );
}

fn read(fs: *mut fs_btrfs_fs, path: &str, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            c(path).as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            len as u64,
        )
    };
    assert!(n >= 0, "read {path}: {}", last_error());
    buf.truncate(n as usize);
    buf
}

#[test]
fn files_truncated_through_the_abi_are_clean_and_the_kernel_agrees() {
    let scratch = Scratch::new("truncate-kernel");
    let image = scratch.image();
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    let multi = read(fs, "/multi", 1 << 20);
    assert_eq!(multi.len(), 196_608, "/multi is not the size made");

    // Inside the second of three extents: the third is released, the
    // second referenced only up to the sector holding the new end.
    assert_eq!(truncate(fs, "/multi", 70_000), 0, "{}", last_error());
    // To nothing: the one extent released.
    assert_eq!(truncate(fs, "/big", 0), 0, "{}", last_error());
    // Inline data cut short.
    assert_eq!(truncate(fs, "/small", 5), 0, "{}", last_error());
    // An empty file grown: the new range is an implicit hole.
    assert_eq!(truncate(fs, "/empty", 1 << 20), 0, "{}", last_error());
    // The same length: nothing changes.
    assert_eq!(truncate(fs, "/shared", 8192), 0, "{}", last_error());

    assert_eq!(read(fs, "/multi", 1 << 20), &multi[..70_000]);
    assert_eq!(read(fs, "/small", 64), b"hello");
    assert_eq!(read(fs, "/empty", 2 << 20), vec![0u8; 1 << 20]);

    // Each refusal names its errno and writes nothing.
    let before = std::fs::read(&image).unwrap();
    refused(fs, "/shared", 0, ENOTSUP);
    refused(fs, "/multi", 100_000, ENOTSUP);
    refused(fs, "/dir", 0, EISDIR);
    refused(fs, "/missing", 0, ENOENT);
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused truncate wrote to the image"
    );
    unsafe { fs_btrfs_umount(fs) };

    let ro = unsafe { fs_btrfs_mount(c(&image).as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    refused(ro, "/multi", 0, EROFS);
    unsafe { fs_btrfs_umount(ro) };
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a read-only handle wrote to the image"
    );

    assert_btrfs_check_clean(
        Path::new(&image),
        "after files were truncated through the ABI",
    );
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let seen = guest_kernel_read_ok(
        &image,
        "after the ABI's truncates",
        "cd \"$MNT\"\n\
         for f in multi big small empty shared; do \
           printf '%s %s %s\\n' \"$f\" \"$(stat -c %s \"$f\")\" \
             \"$(sha256sum \"$f\" | cut -d' ' -f1)\"; done",
    );
    let want: String = [
        ("multi", multi[..70_000].to_vec()),
        ("big", Vec::new()),
        ("small", b"hello".to_vec()),
        ("empty", vec![0u8; 1 << 20]),
    ]
    .iter()
    .map(|(name, bytes)| format!("{name} {} {}\n", bytes.len(), sha256_hex(bytes)))
    .collect();
    let mut lines: Vec<&str> = seen.lines().collect();
    let shared = lines.pop().unwrap_or_default();
    assert_eq!(
        lines.join("\n") + "\n",
        want,
        "the kernel sees the truncated files differently"
    );
    assert!(
        shared.starts_with("shared 8192 "),
        "the kernel sees /shared, which nothing truncated, differently: {shared}"
    );

    // Every sector the kernel can reach, against its digest.
    guest_kernel_write_ok(&image, "scrub", "btrfs scrub start -B \"$MNT\" >/dev/null");

    // The kernel writes to and truncates the same files, and the checker
    // looks again.
    guest_kernel_write_ok(
        &image,
        "the kernel changes the truncated files",
        "cd \"$MNT\"\n\
         head -c 5000 /dev/urandom >> multi\n\
         truncate -s 4096 big\n\
         echo more >> small\n\
         head -c 4096 /dev/urandom | dd of=empty bs=4096 seek=10 conv=notrunc status=none\n\
         truncate -s 1000 multi\n\
         sync",
    );
    assert_btrfs_check_clean(
        Path::new(&image),
        "after the kernel changed the truncated files",
    );
}
