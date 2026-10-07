//! New and removed names, made through the C ABI, judged by what is not
//! this crate (#262).
//!
//! A kernel-made volume gets directories, empty files, a symbolic link
//! and hard links added, and names removed, each call one committed
//! transaction. Then `btrfs check` inspects the whole volume — a
//! directory size that disagrees with its entries, a `DIR_ITEM` without
//! its `DIR_INDEX`, an inode reference naming the wrong index, or a link
//! count that does not match the names are each reported there — and the
//! kernel mounts it and reports what it sees. Last, the kernel itself
//! creates and removes names in the directories this crate made and
//! `btrfs check` looks again: an index or inode counter the kernel
//! derives from what we wrote, and gets wrong, shows up there.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// Errno values as the header documents them, spelled out.
const ENOENT: i32 = 2;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const ENOTEMPTY: i32 = if cfg!(target_os = "macos") { 66 } else { 39 };
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

/// A scratch volume, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str, fill: &str) -> Self {
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
        guest_kernel_write_ok(&image.to_string_lossy(), name, fill);
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

/// What the kernel wrote before this crate touched the volume.
const FILL: &str = "mkdir \"$MNT/d\"\n\
     head -c 20000 /dev/urandom > \"$MNT/d/f\"\n\
     touch \"$MNT/e\"\n\
     mkdir \"$MNT/nc\"\n\
     chattr +C \"$MNT/nc\"\n\
     head -c 9000 /dev/urandom > \"$MNT/data.bin\"\n\
     sync";

fn create(fs: *mut fs_btrfs_fs, path: &str, mode: u32) -> u64 {
    let ino = unsafe { fs_btrfs_create(fs, c(path).as_ptr(), mode) };
    assert_ne!(ino, 0, "create {path}: {}", last_error());
    ino
}

fn mkdir(fs: *mut fs_btrfs_fs, path: &str, mode: u32) -> u64 {
    let ino = unsafe { fs_btrfs_mkdir(fs, c(path).as_ptr(), mode) };
    assert_ne!(ino, 0, "mkdir {path}: {}", last_error());
    ino
}

fn ok(rc: i32, what: &str) {
    assert_eq!(rc, 0, "{what}: {}", last_error());
}

/// `rc` is the failure value and the errno is `want`.
#[track_caller]
fn refused(failed: bool, want: i32, what: &str) {
    assert!(failed, "{what} succeeded and should not have");
    assert_eq!(
        fs_btrfs_last_errno(),
        want,
        "{what}: wrong errno for {:?}",
        last_error()
    );
}

#[test]
fn names_made_and_removed_through_the_abi_are_clean_and_the_kernel_agrees() {
    let scratch = Scratch::new("namespace-kernel", FILL);
    let image = scratch.image();
    let device = c(image.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount_rw(device.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    mkdir(fs, "/new", 0o755);
    mkdir(fs, "/new/sub", 0o700);
    create(fs, "/new/file.txt", 0o644);
    create(fs, "/nc/inherit", 0o600);
    let target: *const c_char = c("file.txt").into_raw();
    let link = unsafe { fs_btrfs_symlink(fs, target, c("/new/link").as_ptr()) };
    drop(unsafe { CString::from_raw(target.cast_mut()) });
    assert_ne!(link, 0, "symlink: {}", last_error());
    ok(
        unsafe { fs_btrfs_link(fs, c("/d/f").as_ptr(), c("/new/hard").as_ptr()) },
        "link /d/f /new/hard",
    );
    ok(
        unsafe { fs_btrfs_link(fs, c("/d/f").as_ptr(), c("/x").as_ptr()) },
        "link /d/f /x",
    );
    ok(
        unsafe { fs_btrfs_unlink(fs, c("/x").as_ptr()) },
        "unlink /x",
    );
    ok(
        unsafe { fs_btrfs_unlink(fs, c("/e").as_ptr()) },
        "unlink /e",
    );
    create(fs, "/gone", 0o644);
    ok(
        unsafe { fs_btrfs_unlink(fs, c("/gone").as_ptr()) },
        "unlink /gone",
    );
    mkdir(fs, "/tmpdir", 0o755);
    ok(
        unsafe { fs_btrfs_rmdir(fs, c("/tmpdir").as_ptr()) },
        "rmdir /tmpdir",
    );

    // Each refusal names its errno and writes nothing.
    let before = std::fs::read(&image).unwrap();
    refused(
        unsafe { fs_btrfs_create(fs, c("/new").as_ptr(), 0o644) } == 0,
        EEXIST,
        "create over a directory",
    );
    refused(
        unsafe { fs_btrfs_rmdir(fs, c("/new").as_ptr()) } != 0,
        ENOTEMPTY,
        "rmdir of a directory with entries",
    );
    refused(
        unsafe { fs_btrfs_unlink(fs, c("/new").as_ptr()) } != 0,
        EISDIR,
        "unlink of a directory",
    );
    refused(
        unsafe { fs_btrfs_rmdir(fs, c("/d/f").as_ptr()) } != 0,
        ENOTDIR,
        "rmdir of a file",
    );
    refused(
        unsafe { fs_btrfs_mkdir(fs, c("/none/sub").as_ptr(), 0o755) } == 0,
        ENOENT,
        "mkdir under a missing directory",
    );
    refused(
        unsafe { fs_btrfs_unlink(fs, c("/data.bin").as_ptr()) } != 0,
        ENOTSUP,
        "unlink of the last name of a file holding data",
    );
    refused(
        unsafe { fs_btrfs_link(fs, c("/new").as_ptr(), c("/new2").as_ptr()) } != 0,
        EISDIR,
        "link of a directory",
    );
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused change wrote to the image"
    );
    unsafe { fs_btrfs_umount(fs) };

    assert_btrfs_check_clean(&image, "after names were made and removed through the ABI");

    let seen = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "after the ABI's names",
        "cd \"$MNT\"\n\
         for p in new new/sub new/file.txt nc/inherit new/link; do \
           stat -c '%n|%F|%a|%h|%s' \"$p\"; done\n\
         stat -c '%n|%h|%s' new/hard d/f\n\
         readlink new/link\n\
         cmp -s new/hard d/f && echo same-bytes\n\
         lsattr -d nc/inherit | cut -d' ' -f1 | grep -q C && echo nodatacow\n\
         for p in e x gone tmpdir; do test -e \"$p\" && echo \"$p exists\" || echo \"$p absent\"; done\n\
         ls -A new | tr '\\n' ' '; echo",
    );
    let want = "new|directory|755|1|38\n\
                new/sub|directory|700|1|0\n\
                new/file.txt|regular empty file|644|1|0\n\
                nc/inherit|regular empty file|600|1|0\n\
                new/link|symbolic link|777|1|8\n\
                new/hard|2|20000\n\
                d/f|2|20000\n\
                file.txt\n\
                same-bytes\n\
                nodatacow\n\
                e absent\n\
                x absent\n\
                gone absent\n\
                tmpdir absent\n\
                file.txt hard link sub \n";
    assert_eq!(seen, want, "the kernel sees a different namespace");

    // The kernel builds on what we made, and the checker looks again.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel changes our directories",
        "cd \"$MNT\"\n\
         echo from-the-kernel > new/sub/k.txt\n\
         mkdir new/sub/kd\n\
         rm new/file.txt\n\
         rmdir new/sub/kd\n\
         ln -s k.txt new/sub/l2\n\
         ln new/hard new/hard2\n\
         sync",
    );
    assert_btrfs_check_clean(&image, "after the kernel changed our directories");
}

/// A name in a directory this crate cannot add to is refused, and the
/// image is left byte for byte as it was. A read-only handle refuses too.
#[test]
fn a_read_only_handle_refuses_every_name_change() {
    let scratch = Scratch::new("namespace-readonly", FILL);
    let image = scratch.image();
    let before = std::fs::read(&image).unwrap();
    let fs = unsafe { fs_btrfs_mount(c(image.to_str().unwrap()).as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_error());
    const EROFS: i32 = 30;
    refused(
        unsafe { fs_btrfs_mkdir(fs, c("/ro").as_ptr(), 0o755) } == 0,
        EROFS,
        "mkdir on a read-only handle",
    );
    refused(
        unsafe { fs_btrfs_unlink(fs, c("/e").as_ptr()) } != 0,
        EROFS,
        "unlink on a read-only handle",
    );
    unsafe { fs_btrfs_umount(fs) };
    assert!(
        std::fs::read(Path::new(&image)).unwrap() == before,
        "a read-only handle wrote to the image"
    );
}
