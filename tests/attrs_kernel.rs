//! Extended attributes, ACLs, mode, owner and times set through the C
//! ABI, judged by what is not this crate (#263).
//!
//! A kernel-made volume holds a file and a directory with attributes the
//! kernel set. Through the ABI an attribute is added, one replaced, one
//! removed, an empty and a binary one stored, an ACL written, and the
//! mode, owner and times changed — each call one committed transaction.
//! Then `btrfs check` inspects the whole volume, and the kernel mounts it
//! and reports, through its own system calls, every attribute's name and
//! value, the ACL as `getfacl` parses it, and each inode's mode, owner
//! and times to the nanosecond. Last, the kernel itself changes the same
//! inodes' attributes and `btrfs check` looks again.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use std::ffi::{c_void, CStr, CString};
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
const EINVAL: i32 = 22;
const EROFS: i32 = 30;
const ERANGE: i32 = 34;

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
            "umask 022\n\
             echo data > \"$MNT/f\"\n\
             setfattr -n user.keep -v one \"$MNT/f\"\n\
             setfattr -n user.replace -v old \"$MNT/f\"\n\
             setfattr -n user.drop -v gone \"$MNT/f\"\n\
             mkdir \"$MNT/d\"\n\
             echo more > \"$MNT/g\"\n\
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

fn ok(rc: i32, what: &str) {
    assert_eq!(rc, 0, "{what}: {}", last_error());
}

#[track_caller]
fn refused(rc: i32, want: i32, what: &str) {
    assert_eq!(rc, -1, "{what} succeeded and should not have");
    assert_eq!(
        fs_btrfs_last_errno(),
        want,
        "{what}: wrong errno for {:?}",
        last_error()
    );
}

fn setxattr(fs: *mut fs_btrfs_fs, path: &str, name: &str, value: &[u8]) -> i32 {
    unsafe {
        fs_btrfs_setxattr(
            fs,
            c(path).as_ptr(),
            c(name).as_ptr(),
            value.as_ptr().cast::<c_void>(),
            value.len(),
        )
    }
}

/// An access ACL in the kernel's xattr encoding: version 2, then
/// (tag, permissions, id) entries sorted by tag and id.
fn acl() -> Vec<u8> {
    const UNDEFINED: u32 = u32::MAX;
    let entries: [(u16, u16, u32); 5] = [
        (0x01, 6, UNDEFINED), // user::rw-
        (0x02, 4, 1234),      // user:1234:r--
        (0x04, 4, UNDEFINED), // group::r--
        (0x10, 4, UNDEFINED), // mask::r--
        (0x20, 0, UNDEFINED), // other::---
    ];
    let mut b = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in entries {
        b.extend(tag.to_le_bytes());
        b.extend(perm.to_le_bytes());
        b.extend(id.to_le_bytes());
    }
    b
}

/// What the kernel reports for f, d and g: mode, owner, times in
/// nanoseconds, then every attribute and its value in hex; and the ACL as
/// `getfacl` parses it.
const KERNEL_VIEW: &str = r#"cd "$MNT"
python3 - <<'PY'
import os
for p in ['f', 'd', 'g']:
    st = os.lstat(p)
    print(p, oct(st.st_mode), st.st_uid, st.st_gid, st.st_atime_ns, st.st_mtime_ns)
    for n in sorted(os.listxattr(p, follow_symlinks=False)):
        print(' ', n, os.getxattr(p, n, follow_symlinks=False).hex() or '-')
PY
getfacl -n --omit-header f
"#;

#[test]
fn attributes_set_through_the_abi_are_clean_and_the_kernel_reads_them() {
    let scratch = Scratch::new("attrs-kernel");
    let image = scratch.image();
    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    ok(
        setxattr(fs, "/f", "user.new", b"fresh"),
        "setxattr user.new",
    );
    ok(
        setxattr(fs, "/f", "user.replace", b"a longer new value"),
        "setxattr user.replace",
    );
    ok(
        unsafe { fs_btrfs_removexattr(fs, c("/f").as_ptr(), c("user.drop").as_ptr()) },
        "removexattr user.drop",
    );
    ok(setxattr(fs, "/f", "user.empty", b""), "setxattr user.empty");
    ok(
        setxattr(fs, "/f", "system.posix_acl_access", &acl()),
        "setxattr the access ACL",
    );
    ok(
        setxattr(fs, "/d", "user.binary", &[0, 0xff, 0x80, 0x0a]),
        "setxattr user.binary",
    );
    ok(
        unsafe { fs_btrfs_chmod(fs, c("/f").as_ptr(), 0o640) },
        "chmod /f",
    );
    ok(
        unsafe { fs_btrfs_chmod(fs, c("/g").as_ptr(), 0o4751) },
        "chmod /g",
    );
    ok(
        unsafe { fs_btrfs_chown(fs, c("/g").as_ptr(), 1234, 5678) },
        "chown /g",
    );
    ok(
        unsafe { fs_btrfs_chown(fs, c("/d").as_ptr(), u32::MAX, 99) },
        "chown /d (group only)",
    );
    ok(
        unsafe {
            fs_btrfs_utimens(
                fs,
                c("/g").as_ptr(),
                1_000_000_000,
                123_456_789,
                1_234_567_890,
                500_000_000,
            )
        },
        "utimens /g",
    );

    // The ABI reads back what it wrote.
    let mut buf = [0u8; 64];
    let n = unsafe {
        fs_btrfs_getxattr(
            fs,
            c("/f").as_ptr(),
            c("user.replace").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            buf.len(),
        )
    };
    assert_eq!(
        &buf[..n.max(0) as usize],
        b"a longer new value",
        "{}",
        last_error()
    );

    // Each refusal names its errno and writes nothing.
    let before = std::fs::read(&image).unwrap();
    refused(
        unsafe { fs_btrfs_removexattr(fs, c("/f").as_ptr(), c("user.absent").as_ptr()) },
        ENOENT,
        "removexattr of an attribute that is not there",
    );
    refused(
        setxattr(fs, "/missing", "user.x", b"1"),
        ENOENT,
        "setxattr on a missing path",
    );
    refused(
        setxattr(fs, "/f", "", b"1"),
        EINVAL,
        "setxattr with an empty name",
    );
    refused(
        setxattr(fs, "/f", &format!("user.{}", "n".repeat(251)), b"1"),
        ERANGE,
        "setxattr with a 256-byte name",
    );
    refused(
        unsafe { fs_btrfs_utimens(fs, c("/g").as_ptr(), 0, 1_000_000_000, 0, 0) },
        EINVAL,
        "utimens with a whole second of nanoseconds",
    );
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused change wrote to the image"
    );
    unsafe { fs_btrfs_umount(fs) };

    // A read-only handle refuses, and writes nothing.
    let ro = unsafe { fs_btrfs_mount(c(&image).as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    refused(
        setxattr(ro, "/f", "user.ro", b"1"),
        EROFS,
        "setxattr read-only",
    );
    refused(
        unsafe { fs_btrfs_chmod(ro, c("/f").as_ptr(), 0o600) },
        EROFS,
        "chmod read-only",
    );
    unsafe { fs_btrfs_umount(ro) };
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a read-only handle wrote to the image"
    );

    assert_btrfs_check_clean(
        std::path::Path::new(&image),
        "after attributes were set through the ABI",
    );

    let seen = guest_kernel_read_ok(&image, "after the ABI's attributes", KERNEL_VIEW);
    let mut lines = seen.lines();
    let f = lines.next().unwrap_or_default().to_string();
    assert!(
        f.starts_with("f 0o100640 0 0 "),
        "the kernel sees f's mode or owner differently: {f}\n{seen}"
    );
    let rest: Vec<&str> = lines.collect();
    let want_attrs = [
        format!("  system.posix_acl_access {}", hex(&acl())),
        "  user.empty -".to_string(),
        format!("  user.keep {}", hex(b"one")),
        format!("  user.new {}", hex(b"fresh")),
        format!("  user.replace {}", hex(b"a longer new value")),
    ];
    assert_eq!(
        &rest[..want_attrs.len()],
        &want_attrs.iter().map(String::as_str).collect::<Vec<_>>()[..],
        "the kernel sees f's attributes differently:\n{seen}"
    );
    let d = rest[want_attrs.len()];
    assert!(
        d.starts_with("d 0o40755 0 99 "),
        "the kernel sees d's mode or owner differently: {d}\n{seen}"
    );
    assert_eq!(
        rest[want_attrs.len() + 1],
        "  user.binary 00ff800a",
        "the kernel sees d's binary attribute differently:\n{seen}"
    );
    assert_eq!(
        rest[want_attrs.len() + 2],
        "g 0o104751 1234 5678 1000000000123456789 1234567890500000000",
        "the kernel sees g's mode, owner or times differently:\n{seen}"
    );
    let acl_lines: Vec<&str> = rest[want_attrs.len() + 3..]
        .iter()
        .copied()
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        acl_lines,
        [
            "user::rw-",
            "user:1234:r--",
            "group::r--",
            "mask::r--",
            "other::---"
        ],
        "getfacl parses the ACL differently:\n{seen}"
    );

    // The kernel builds on what we wrote, and the checker looks again.
    guest_kernel_write_ok(
        &image,
        "the kernel changes the same attributes",
        "cd \"$MNT\"\n\
         setfattr -n user.new -v from-the-kernel f\n\
         setfattr -x user.keep f\n\
         setfattr -n user.k2 -v 2 d\n\
         chmod 600 g\n\
         touch g\n\
         sync",
    );
    assert_btrfs_check_clean(
        std::path::Path::new(&image),
        "after the kernel changed the same attributes",
    );
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
