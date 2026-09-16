//! A non-UTF-8 attribute name that `listxattr` returns reads back through
//! `getxattr` with the same bytes (#104).
//!
//! The checked-in xattr fixture's names are all UTF-8, so its round trip
//! holds with or without the fix. This test makes its own image instead:
//! a file carrying `user.caf\xe9\xff` on the host, copied in by
//! `mkfs.btrfs --rootdir`, which needs no mount and no root.
//!
//! It needs `mkfs.btrfs` and `setfattr`, and a host filesystem that takes
//! `user.` attributes. Without those it skips, unless
//! `BTRFS_ORACLE_FIXTURES=required`: the CI job that installs btrfs-progs
//! sets that, so a missing prerequisite there fails rather than reading as
//! a pass.

use fs_btrfs::capi::*;
use std::ffi::{c_char, CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

const NAME: &[u8] = b"user.caf\xe9\xff";
const VALUE: &[u8] = b"read back by exact bytes";

fn required() -> bool {
    std::env::var("BTRFS_ORACLE_FIXTURES").is_ok_and(|v| v == "required")
}

fn skip(why: String) -> Option<std::path::PathBuf> {
    assert!(!required(), "BTRFS_ORACLE_FIXTURES=required, but {why}");
    eprintln!("skipping: {why}");
    None
}

/// An image whose `/f` carries [`NAME`] = [`VALUE`].
fn image() -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("btrfs-xattr-bytes-{}", std::process::id()));
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("f");
    std::fs::write(&file, b"f").unwrap();
    let mut value_arg = b"0x".to_vec();
    value_arg.extend(VALUE.iter().flat_map(|b| format!("{b:02x}").into_bytes()));
    match Command::new("setfattr")
        .arg("-n")
        .arg(OsStr::from_bytes(NAME))
        .arg("-v")
        .arg(OsStr::from_bytes(&value_arg))
        .arg(&file)
        .output()
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return skip(format!(
                "setfattr failed: {}",
                String::from_utf8_lossy(&o.stderr)
            ))
        }
        Err(e) => return skip(format!("setfattr not runnable: {e}")),
    }
    let img = dir.join("img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    match Command::new("mkfs.btrfs")
        .arg("-f")
        .arg("--rootdir")
        .arg(&root)
        .arg(&img)
        .output()
    {
        Ok(o) if o.status.success() => Some(img),
        Ok(o) => panic!("mkfs.btrfs failed: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => skip(format!("mkfs.btrfs not runnable: {e}")),
    }
}

fn last_error() -> String {
    let p = fs_btrfs_last_error();
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[test]
fn a_listed_non_utf8_name_reads_back_by_its_bytes() {
    let Some(img) = image() else { return };
    let img_c = CString::new(img.as_os_str().as_bytes()).unwrap();
    let fs = unsafe { fs_btrfs_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_error());
    let path = CString::new("/f").unwrap();

    let needed = unsafe { fs_btrfs_listxattr(fs, path.as_ptr(), std::ptr::null_mut(), 0) };
    assert!(needed > 0, "{}", last_error());
    let mut list = vec![0u8; needed as usize];
    let got = unsafe {
        fs_btrfs_listxattr(
            fs,
            path.as_ptr(),
            list.as_mut_ptr().cast::<c_char>(),
            list.len(),
        )
    };
    assert_eq!(got, needed);
    let listed = list
        .split(|&b| b == 0)
        .find(|n| *n == NAME)
        .unwrap_or_else(|| {
            panic!(
                "listxattr did not return the name: {:?}",
                String::from_utf8_lossy(&list)
            )
        });

    // The exact bytes listxattr handed out, handed straight back.
    let name = CString::new(listed.to_vec()).unwrap();
    let mut buf = vec![0u8; 64];
    let n = unsafe {
        fs_btrfs_getxattr(
            fs,
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    assert_eq!(n, VALUE.len() as i64, "getxattr: {}", last_error());
    assert_eq!(&buf[..VALUE.len()], VALUE);
    unsafe { fs_btrfs_umount(fs) };
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}
