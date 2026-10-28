//! `fs.btrfs defragment` (#305): the library's defragment reached from the
//! command line, on this crate's own `mkfs.btrfs` volume.
//!
//! The tool's change is held to the library's own on a twin copy of the
//! image: the two must come out byte-identical, and the report must count
//! what the library counted. Each refusal leaves the image byte-identical.
//! What the kernel and `btrfs check` make of a file the tool defragments
//! is tests/cli_defrag_kernel.rs.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use fs_btrfs::Filesystem;
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The appends the fragmented file is made of.
const APPENDS: usize = 5;
const APPEND: usize = 8192;

fn pattern(append: usize) -> Vec<u8> {
    (0..APPEND)
        .map(|i| (i as u8).wrapping_mul(13) ^ (append as u8).wrapping_mul(29))
        .collect()
}

fn mount_rw(img: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(img).unwrap());
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap()
}

/// A fresh 128 MiB volume made by this crate's `mkfs.btrfs`, holding
/// `frag`, written as [`APPENDS`] appends committed one at a time so each
/// is an extent of its own; `whole`, written in one go; and a directory,
/// `dir`.
fn volume(tag: &str) -> PathBuf {
    let img = scratch_dir(tag).join("defrag.img");
    ok(tool("mkfs.btrfs")
        .args(["-q", "--text", "--size", "128M"])
        .arg(&img));
    let mut fs = mount_rw(&img);
    let root = fs.lookup_path("/").unwrap().ino;
    let frag = fs.create(root, b"frag", 0o644, 0, 0).unwrap();
    for n in 0..APPENDS {
        fs.write(frag, (n * APPEND) as u64, &pattern(n)).unwrap();
    }
    let whole = fs.create(root, b"whole", 0o644, 0, 0).unwrap();
    fs.write(whole, 0, &vec![0xa5; 2 * APPEND]).unwrap();
    fs.mkdir(root, b"dir", 0o755, 0, 0).unwrap();
    img
}

fn twin(img: &Path) -> PathBuf {
    let twin = img.with_file_name("twin.img");
    std::fs::copy(img, &twin).unwrap();
    twin
}

fn fs_btrfs(img: &Path) -> std::process::Command {
    let mut cmd = tool("fs.btrfs");
    cmd.arg(img);
    cmd
}

fn read(img: &Path, path: &str) -> Vec<u8> {
    let fs = mount_rw(img);
    let ino = fs.lookup_path(path).unwrap().ino;
    fs.read_file(ino).unwrap()
}

#[test]
fn defragment_help_and_man_page_describe_the_verb() {
    let help = stdout(&ok(tool("fs.btrfs").args(["image.img", "--help"])));
    assert!(
        help.contains("defragment"),
        "--help omits defragment: {help}"
    );
    let verb = stdout(&ok(tool("fs.btrfs").args([
        "image.img",
        "defragment",
        "--help",
    ])));
    for word in ["PATH", "--target", "Examples:"] {
        assert!(
            verb.contains(word),
            "defragment --help omits {word}: {verb}"
        );
    }
    let share = scratch_dir("defragment-docs");
    ok(entry().args(["generate", "man"]).arg(&share));
    let page = share.join("man/man1/fs.btrfs-defragment.1");
    assert!(page.is_file(), "no {}", page.display());
}

#[test]
fn defragment_reports_what_the_library_did_and_leaves_its_image() {
    let img = volume("defragment");
    let copy = twin(&img);
    let want: Vec<u8> = (0..APPENDS).flat_map(pattern).collect();

    let report = stdout(&ok(fs_btrfs(&img).args(["defragment", "/frag"])));
    assert_eq!(json_field(&report, "path"), "/frag", "{report}");
    assert_eq!(
        json_field(&report, "items_before"),
        APPENDS.to_string(),
        "{report}"
    );
    assert_eq!(json_field(&report, "items_after"), "1", "{report}");
    assert_eq!(json_field(&report, "runs"), "1", "{report}");
    assert_eq!(
        json_field(&report, "bytes"),
        (APPENDS * APPEND).to_string(),
        "{report}"
    );
    assert_eq!(read(&img, "/frag"), want, "/frag after the defragment");

    let mut lib = mount_rw(&copy);
    let ino = lib.lookup_path("/frag").unwrap().ino;
    let done = lib.defragment(ino).unwrap();
    drop(lib);
    assert_eq!((done.items_before, done.items_after), (APPENDS, 1));
    assert!(
        std::fs::read(&img).unwrap() == std::fs::read(&copy).unwrap(),
        "fs.btrfs defragment and the library's defragment left different images"
    );

    // Nothing left to do: reported, and nothing written.
    let before = std::fs::read(&img).unwrap();
    let report = stdout(&ok(fs_btrfs(&img).args(["defragment", "/frag"])));
    assert_eq!(json_field(&report, "runs"), "0", "{report}");
    let text = stdout(&ok(fs_btrfs(&img).args(["--text", "defragment", "/whole"])));
    assert!(text.contains("/whole"), "{text}");
    assert!(
        std::fs::read(&img).unwrap() == before,
        "a defragment with nothing to do wrote to the image"
    );
}

#[test]
fn a_target_bounds_each_run_as_defragment_to_does() {
    let img = volume("defragment-target");
    let copy = twin(&img);

    let report = stdout(&ok(fs_btrfs(&img).args([
        "defragment",
        "--target",
        "16K",
        "/frag",
    ])));
    // Five appends of 8 KiB into runs of at most 16 KiB: two runs of two,
    // and the last append left alone.
    assert_eq!(json_field(&report, "runs"), "2", "{report}");
    assert_eq!(json_field(&report, "items_after"), "3", "{report}");

    let mut lib = mount_rw(&copy);
    let ino = lib.lookup_path("/frag").unwrap().ino;
    lib.defragment_to(ino, 16 << 10).unwrap();
    drop(lib);
    assert!(
        std::fs::read(&img).unwrap() == std::fs::read(&copy).unwrap(),
        "fs.btrfs defragment --target and the library's defragment_to left different images"
    );
}

#[test]
fn refusals_name_their_reason_and_leave_the_image_as_it_was() {
    let img = volume("defragment-refusals");
    let before = std::fs::read(&img).unwrap();

    // The volume's state refuses: a path that is not there, or one that is
    // not a regular file.
    let message = refused(fs_btrfs(&img).args(["defragment", "/missing"]), 4);
    assert!(message.contains("/missing"), "{message}");
    let message = refused(fs_btrfs(&img).args(["defragment", "/dir"]), 4);
    assert!(message.contains("/dir"), "{message}");

    // The library refuses: a target shorter than a sector.
    let message = refused(
        fs_btrfs(&img).args(["defragment", "--target", "512", "/frag"]),
        3,
    );
    assert!(message.contains("sector"), "{message}");

    // The command line is wrong: not a size, or no path.
    refused(
        fs_btrfs(&img).args(["defragment", "--target", "lots", "/frag"]),
        2,
    );
    refused(
        fs_btrfs(&img).args(["defragment", "--target", "0", "/frag"]),
        2,
    );
    refused(fs_btrfs(&img).args(["defragment"]), 2);

    assert!(
        std::fs::read(&img).unwrap() == before,
        "a refused defragment changed the image"
    );
}
