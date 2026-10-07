//! Names moved through the C ABI, judged by what is not this crate
//! (#262).
//!
//! A kernel-made volume has names moved within a directory and between
//! directories, a directory moved with what it holds, and a file, a hard
//! link and an empty directory replaced at the destination — each call
//! one committed transaction. Then `btrfs check` inspects the whole
//! volume, which reports a directory size that disagrees with its
//! entries, a `DIR_ITEM` without its `DIR_INDEX`, an inode reference
//! naming the wrong parent or index, and a link count that does not match
//! the names. The kernel mounts it and lists every path, type, link count
//! and size, reads the moved bytes back, and resolves inode numbers back
//! to paths through the inode references (`btrfs inspect-internal
//! inode-resolve`, the kernel's own back-reference walk). Last, the
//! kernel moves names in the same directories and `btrfs check` looks
//! again.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use std::ffi::{CStr, CString};
use std::path::PathBuf;

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

const ENOENT: i32 = 2;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;
const ENOTEMPTY: i32 = if cfg!(target_os = "macos") { 66 } else { 39 };
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
            "cd \"$MNT\"\n\
             mkdir d1 d2 d3 d1/sub d3/emptydir d3/full\n\
             echo a > d1/sub/inner\n\
             echo x > d1/a\n\
             echo y > d2/b\n\
             touch d2/empty d3/full/x keep\n\
             head -c 20000 /dev/urandom > big\n\
             ln big d3/big-link\n\
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

fn rename(fs: *mut fs_btrfs_fs, from: &str, to: &str) -> i32 {
    unsafe { fs_btrfs_rename(fs, c(from).as_ptr(), c(to).as_ptr()) }
}

fn ok(fs: *mut fs_btrfs_fs, from: &str, to: &str) {
    assert_eq!(
        rename(fs, from, to),
        0,
        "rename {from} {to}: {}",
        last_error()
    );
}

#[track_caller]
fn refused(fs: *mut fs_btrfs_fs, from: &str, to: &str, want: i32) {
    assert_eq!(
        rename(fs, from, to),
        -1,
        "rename {from} {to} succeeded and should not have"
    );
    assert_eq!(
        fs_btrfs_last_errno(),
        want,
        "rename {from} {to}: wrong errno for {:?}",
        last_error()
    );
}

#[test]
fn names_moved_through_the_abi_are_clean_and_the_kernel_agrees() {
    let scratch = Scratch::new("rename-kernel");
    let image = scratch.image();
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    // Within one directory, and a directory with what it holds to another.
    ok(fs, "/d1/a", "/d1/a2");
    ok(fs, "/d1/sub", "/d2/sub");
    // Replacing an empty file in the same directory: it goes.
    ok(fs, "/d2/b", "/d2/empty");
    // A file holding data, with a second name, to another directory.
    ok(fs, "/big", "/d3/moved-big");
    // Replacing one of two names of a file: it keeps the other.
    ok(fs, "/d1/a2", "/d3/big-link");
    // Replacing an empty directory with a directory.
    ok(fs, "/d2/sub", "/d3/emptydir");
    // Onto itself: nothing happens.
    ok(fs, "/keep", "/keep");

    // Each refusal names its errno and writes nothing.
    let before = std::fs::read(&image).unwrap();
    refused(fs, "/d3", "/d3/full/inside", EINVAL);
    refused(fs, "/d3/emptydir", "/d3/full", ENOTEMPTY);
    refused(fs, "/keep", "/d3", EISDIR);
    refused(fs, "/d3/full", "/keep", ENOTDIR);
    refused(fs, "/nope", "/x", ENOENT);
    refused(fs, "/keep", "/d1/missing/x", ENOENT);
    // The replaced file's last name, while it still holds data.
    refused(fs, "/keep", "/d3/moved-big", ENOTSUP);
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused rename wrote to the image"
    );
    unsafe { fs_btrfs_umount(fs) };

    let ro = unsafe { fs_btrfs_mount(c(&image).as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    refused(ro, "/keep", "/kept", EROFS);
    unsafe { fs_btrfs_umount(ro) };
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a read-only handle wrote to the image"
    );

    assert_btrfs_check_clean(
        std::path::Path::new(&image),
        "after names were moved through the ABI",
    );

    let seen = guest_kernel_read_ok(
        &image,
        "after the ABI's renames",
        "cd \"$MNT\"\n\
         find . | LC_ALL=C sort | while read -r p; do stat -c '%n|%F|%h|%s' \"$p\"; done\n\
         cat d2/empty d3/big-link d3/emptydir/inner\n\
         for p in d3/emptydir/inner d3/big-link d3/moved-big d3/emptydir; do \
           btrfs inspect-internal inode-resolve \"$(stat -c %i \"$p\")\" \"$MNT\" \
             | sed \"s|^$MNT/||\"; done",
    );
    let want = ".|directory|1|20\n\
                ./d1|directory|1|0\n\
                ./d2|directory|1|10\n\
                ./d2/empty|regular file|1|2\n\
                ./d3|directory|1|58\n\
                ./d3/big-link|regular file|1|2\n\
                ./d3/emptydir|directory|1|10\n\
                ./d3/emptydir/inner|regular file|1|2\n\
                ./d3/full|directory|1|2\n\
                ./d3/full/x|regular empty file|1|0\n\
                ./d3/moved-big|regular file|1|20000\n\
                ./keep|regular empty file|1|0\n\
                y\n\
                x\n\
                a\n\
                d3/emptydir/inner\n\
                d3/big-link\n\
                d3/moved-big\n\
                d3/emptydir\n";
    assert_eq!(seen, want, "the kernel sees a different namespace");

    // The kernel moves names in the directories we changed, and the
    // checker looks again.
    guest_kernel_write_ok(
        &image,
        "the kernel moves names in our directories",
        "cd \"$MNT\"\n\
         mv d3/emptydir/inner d1/back\n\
         mv d2/empty d3/emptydir/again\n\
         mv d3/emptydir d2/home\n\
         rm d3/full/x\n\
         rmdir d3/full\n\
         touch d1/new\n\
         sync",
    );
    assert_btrfs_check_clean(
        std::path::Path::new(&image),
        "after the kernel moved names in our directories",
    );
}
