//! `fs.btrfs xattr`: a path's extended attributes listed as JSON, sorted by
//! name, and one value's raw bytes on stdout or in a file with `-o` (#416).
//! The volume is this crate's own `mkfs.btrfs`'s, its attributes set
//! through the library; what Linux sets is tests/cli_xattr_kernel.rs.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use fs_btrfs::fs::Filesystem;
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

/// `trusted.` and then a byte that is not UTF-8.
const RAW_NAME: &[u8] = b"trusted.\xff";
const RAW_NAME_HEX: &str = "747275737465642eff";

/// A volume holding `/f` with three attributes, one of them named in bytes
/// that are not UTF-8 and one holding no bytes at all, and `/g` with none.
fn volume(tag: &str) -> (PathBuf, PathBuf) {
    let dir = scratch_dir(tag);
    let img = dir.join("xattr.img");
    ok(tool("mkfs.btrfs")
        .args(["-q", "--text", "--size", "128M"])
        .arg(&img));
    for path in ["/f", "/g"] {
        ok(tool("fs.btrfs").arg(&img).args(["create", path]));
    }
    let dev = Arc::new(FileDevice::open_rw(&img).unwrap());
    let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let ino = fs.lookup_path_bytes(b"/f").expect("/f exists").ino;
    fs.set_xattr(ino, b"user.comment", b"twelve bytes")
        .expect("set user.comment");
    fs.set_xattr(ino, RAW_NAME, &[0, 1, 2])
        .expect("set the raw name");
    fs.set_xattr(ino, b"user.empty", b"")
        .expect("set user.empty");
    drop(fs);
    (dir, img)
}

fn xattr(img: &Path, args: &[&str]) -> std::process::Command {
    let mut cmd = tool("fs.btrfs");
    cmd.arg(img).arg("xattr").args(args).stdin(Stdio::null());
    cmd
}

#[test]
fn xattr_lists_a_paths_attributes_sorted_by_name() {
    let (dir, img) = volume("xattr-list");
    let before = std::fs::read(&img).unwrap();
    let list = stdout(&ok(&mut xattr(&img, &["/f"])));

    let comment = list.find("\"name\": \"user.comment\"");
    let empty = list.find("\"name\": \"user.empty\"");
    let raw = list.find(&format!("\"name_hex\": \"{RAW_NAME_HEX}\""));
    assert!(
        comment.is_some() && empty.is_some() && raw.is_some(),
        "every attribute is listed, the raw name with its bytes:\n{list}"
    );
    // By name, as bytes: trusted.\xff, user.comment, user.empty.
    assert!(raw < comment && comment < empty, "sorted by name:\n{list}");
    assert_eq!(
        list.matches("\"name_hex\"").count(),
        1,
        "only the name that is not UTF-8 carries name_hex:\n{list}"
    );
    for size in ["\"size\": 12", "\"size\": 3", "\"size\": 0"] {
        assert!(list.contains(size), "{size} missing:\n{list}");
    }

    // A path with none lists none.
    let none = stdout(&ok(&mut xattr(&img, &["/g"])));
    assert_eq!(none.trim_end(), "[]", "{none}");

    // --text, for a person.
    let mut text = tool("fs.btrfs");
    text.arg("--text").arg(&img).args(["xattr", "/f"]);
    let text = stdout(&ok(&mut text));
    assert!(
        text.lines()
            .any(|l| l.contains("user.comment") && l.contains("12")),
        "{text}"
    );

    assert!(
        std::fs::read(&img).unwrap() == before,
        "xattr changed the image"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn xattr_writes_one_value_as_raw_bytes() {
    let (dir, img) = volume("xattr-value");
    let out = ok(&mut xattr(&img, &["/f", "user.comment"]));
    assert_eq!(out.stdout, b"twelve bytes");

    // A value set to no bytes is there, and empty.
    let out = ok(&mut xattr(&img, &["/f", "user.empty"]));
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);

    // -o FILE, as read does: the bytes in FILE and nothing on stdout.
    let file = dir.join("comment.bin");
    let out = ok(&mut xattr(
        &img,
        &["/f", "user.comment", "-o", file.to_str().unwrap()],
    ));
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    assert_eq!(std::fs::read(&file).unwrap(), b"twelve bytes");
    let mut partial = file.clone().into_os_string();
    partial.push(".partial");
    assert!(
        !Path::new(&partial).exists(),
        "the .partial file was left behind"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn xattr_refuses_a_missing_attribute_or_path_by_name() {
    let (dir, img) = volume("xattr-missing");
    let message = refused(&mut xattr(&img, &["/f", "user.absent"]), 1);
    assert!(message.contains("user.absent"), "{message}");
    let message = refused(&mut xattr(&img, &["/absent"]), 1);
    assert!(message.contains("/absent"), "{message}");
    let message = refused(&mut xattr(&img, &["/absent", "user.comment"]), 1);
    assert!(message.contains("/absent"), "{message}");
    let _ = std::fs::remove_dir_all(&dir);
}
