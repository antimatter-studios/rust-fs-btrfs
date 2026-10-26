//! `fs.btrfs export --recursive` through the shared tree transfer (#541),
//! on this crate's own `mkfs.btrfs` volume with a tree written through the
//! library: nested directories, an extent-backed file, a file with two more
//! names, two symlinks, and times set on files and directories. What Linux
//! makes of a tree 33 directories deep, subvolumes and a damaged file is
//! tests/cli_export_tree_kernel.rs.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::inode::FIRST_FREE_OBJECTID;
use fs_core::{BlockDevice, FileDevice};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Times set on the tree: (seconds, nanoseconds), access then modification.
const FILE_TIMES: ((i64, u32), (i64, u32)) =
    ((981_173_106, 123_456_789), (1_234_567_890, 987_654_321));
const DIR_TIMES: ((i64, u32), (i64, u32)) =
    ((1_000_000_000, 500_000_000), (1_100_000_000, 250_000_000));

fn deep_bytes() -> Vec<u8> {
    (0..300_000u32).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// `/tree` below the root: `top`, its other names `hard` and `a/hard2`,
/// `a/b/c/deep` (the deepest entry, at depth 4), `link` to `top` and
/// `a/up` to `../..`. Times are set on `top`, `a` and `/tree` itself, the
/// directories last so no later entry moves them.
fn volume(tag: &str) -> (PathBuf, PathBuf) {
    let dir = scratch_dir(tag);
    let img = dir.join("tree.img");
    ok(tool("mkfs.btrfs")
        .args(["-q", "--text", "--size", "128M"])
        .arg(&img));
    let dev = Arc::new(FileDevice::open_rw(&img).unwrap());
    let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let tree = fs.mkdir(FIRST_FREE_OBJECTID, b"tree", 0o755, 0, 0).unwrap();
    let a = fs.mkdir(tree, b"a", 0o755, 0, 0).unwrap();
    let b = fs.mkdir(a, b"b", 0o755, 0, 0).unwrap();
    let c = fs.mkdir(b, b"c", 0o755, 0, 0).unwrap();
    let top = fs.create(tree, b"top", 0o644, 0, 0).unwrap();
    fs.write(top, 0, b"top\n").unwrap();
    fs.link(top, tree, b"hard").unwrap();
    fs.link(top, a, b"hard2").unwrap();
    let deep = fs.create(c, b"deep", 0o644, 0, 0).unwrap();
    fs.write(deep, 0, &deep_bytes()).unwrap();
    fs.symlink(tree, b"link", b"top", 0, 0).unwrap();
    fs.symlink(a, b"up", b"../..", 0, 0).unwrap();
    let (atime, mtime) = FILE_TIMES;
    fs.set_times(top, Some(atime), Some(mtime)).unwrap();
    let (atime, mtime) = DIR_TIMES;
    fs.set_times(a, Some(atime), Some(mtime)).unwrap();
    fs.set_times(tree, Some(atime), Some(mtime)).unwrap();
    drop(fs);
    (dir, img)
}

/// `fs.btrfs IMAGE export --recursive FLAGS... /tree DEST`.
fn export_tree(img: &Path, flags: &[&str], dest: &Path) -> std::process::Output {
    tool("fs.btrfs")
        .arg(img)
        .args(["export", "--recursive"])
        .args(flags)
        .arg("/tree")
        .arg(dest)
        .output()
        .unwrap()
}

/// The report's item after its path key, up to the next item's path key.
#[track_caller]
fn item<'a>(report: &'a str, path: &str) -> &'a str {
    let needle = format!("\"path\": \"{path}\"");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("no item for {path} in:\n{report}"));
    let rest = &report[start + needle.len()..];
    &rest[..rest.find("\"path\": ").unwrap_or(rest.len())]
}

fn nanos(t: std::io::Result<std::time::SystemTime>) -> i128 {
    let t = t.unwrap();
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

fn want((sec, nsec): (i64, u32)) -> i128 {
    i128::from(sec) * 1_000_000_000 + i128::from(nsec)
}

#[test]
fn a_tree_is_exported_with_its_links_and_times() {
    let (dir, img) = volume("export-tree");
    let before = std::fs::read(&img).unwrap();

    // Without --recursive a directory is refused, as before.
    let plain = dir.join("plain");
    refused(
        tool("fs.btrfs")
            .arg(&img)
            .args(["export", "/tree"])
            .arg(&plain),
        1,
    );
    assert!(!plain.exists());

    // Symlinks are refused without --preserve=links; the rest is exported.
    let refusing = dir.join("refusing");
    let out = export_tree(&img, &[], &refusing);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "completed"), "8", "{report}");
    assert_eq!(json_field(&report, "refused"), "2", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    for path in ["/tree/link", "/tree/a/up"] {
        let entry = item(&report, path);
        assert!(entry.contains("\"status\": \"refused\""), "{entry}");
        assert!(entry.contains("--preserve=links"), "{entry}");
    }
    assert!(std::fs::symlink_metadata(refusing.join("link")).is_err());
    assert_eq!(std::fs::read(refusing.join("top")).unwrap(), b"top\n");
    assert_eq!(
        std::fs::read(refusing.join("a/b/c/deep")).unwrap(),
        deep_bytes()
    );
    assert_ne!(
        std::fs::metadata(refusing.join("top")).unwrap().ino(),
        std::fs::metadata(refusing.join("hard")).unwrap().ino(),
        "hard links are copies without --preserve=links"
    );

    // Links and times preserved: every symlink as stored, never followed,
    // the three names of `top` one file, and the times set above.
    let dest = dir.join("preserved");
    let out = export_tree(&img, &["--preserve=links,times"], &dest);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "completed"), "10", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    assert_eq!(
        std::fs::read_link(dest.join("link")).unwrap(),
        Path::new("top")
    );
    assert_eq!(
        std::fs::read_link(dest.join("a/up")).unwrap(),
        Path::new("../..")
    );
    assert!(item(&report, "/tree/a/up").contains("target escapes DEST"));
    let top = std::fs::metadata(dest.join("top")).unwrap();
    assert_eq!(top.nlink(), 3, "top, hard and a/hard2 are one file");
    for name in ["hard", "a/hard2"] {
        assert_eq!(std::fs::metadata(dest.join(name)).unwrap().ino(), top.ino());
    }
    assert_eq!(
        std::fs::read(dest.join("a/b/c/deep")).unwrap(),
        deep_bytes()
    );
    for (local, (atime, mtime)) in [
        (dest.join("top"), FILE_TIMES),
        (dest.join("a"), DIR_TIMES),
        (dest.clone(), DIR_TIMES),
    ] {
        let meta = std::fs::metadata(&local).unwrap();
        assert_eq!(nanos(meta.accessed()), want(atime), "{}", local.display());
        assert_eq!(nanos(meta.modified()), want(mtime), "{}", local.display());
    }

    // The depth bound: an exact fit is complete, one level less fails the
    // deepest entry alone.
    let fit = dir.join("fit");
    let out = export_tree(&img, &["--preserve=links", "--max-depth", "4"], &fit);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert_eq!(json_field(&stdout(&out), "completed"), "10");
    let over = dir.join("over");
    let out = export_tree(&img, &["--preserve=links", "--max-depth", "3"], &over);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}");
    assert_eq!(json_field(&report, "completed"), "9", "{report}");
    assert_eq!(json_field(&report, "failed"), "1", "{report}");
    assert!(item(&report, "/tree/a/b/c/deep").contains("maximum depth exceeded"));
    assert!(over.join("a/b/c").is_dir());
    assert!(std::fs::symlink_metadata(over.join("a/b/c/deep")).is_err());

    assert!(
        std::fs::read(&img).unwrap() == before,
        "export changed the image"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
