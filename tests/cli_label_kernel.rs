//! `fs.btrfs set label` writes the label every reader sees (#264).
//!
//! The label lives in the superblock and nowhere else, in every copy of
//! it. After `set label`, the reference tool's superblock dump, the Linux
//! kernel's own `btrfs filesystem label` on a mount, and this crate's
//! reader all report the new label, and `btrfs check` finds nothing wrong.
//! A label too long for its field is refused and the volume left as it
//! was.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{assert_btrfs_check_clean, dump_super, fixture, guest_kernel_read_ok};

fn label_of(dump: &str) -> String {
    dump.lines()
        .find_map(|l| l.strip_prefix("label").map(|v| v.trim().to_string()))
        .unwrap_or_else(|| panic!("dump-super printed no label:\n{dump}"))
}

#[test]
fn a_new_label_is_the_one_the_kernel_and_the_reference_tools_read() {
    let image = scratch_dir("label").join("btrfs-default.img");
    std::fs::copy(fixture("btrfs-default.img"), &image).expect("copy the fixture");

    ok(tool("fs.btrfs")
        .arg(&image)
        .args(["set", "label", "DJ RENAMED"]));

    let got = stdout(&ok(tool("fs.btrfs")
        .arg(&image)
        .args(["get", "label", "--text"])));
    assert_eq!(
        got.trim(),
        "DJ RENAMED",
        "fs.btrfs reads back a different label"
    );
    assert_eq!(
        label_of(&dump_super(&image)),
        "DJ RENAMED",
        "dump-super disagrees"
    );
    assert_btrfs_check_clean(&image, "after set label");
    let kernel = guest_kernel_read_ok(
        image.to_str().expect("a UTF-8 scratch path"),
        "the kernel reading the label",
        "btrfs filesystem label \"$MNT\"\n",
    );
    assert_eq!(
        kernel.trim(),
        "DJ RENAMED",
        "the kernel reads a different label"
    );
}

#[test]
fn a_label_too_long_is_refused_and_nothing_changes() {
    let image = scratch_dir("label-long").join("btrfs-default.img");
    std::fs::copy(fixture("btrfs-default.img"), &image).expect("copy the fixture");
    let before = std::fs::read(&image).unwrap();
    let out = tool("fs.btrfs")
        .arg(&image)
        .args(["set", "label", &"x".repeat(256)])
        .output()
        .expect("spawn fs.btrfs");
    assert!(!out.status.success(), "a 256-byte label was accepted");
    assert!(
        stderr(&out).contains("255"),
        "the refusal does not give the limit: {}",
        stderr(&out)
    );
    assert!(
        std::fs::read(&image).unwrap() == before,
        "the refused label changed the image"
    );
}

#[test]
fn a_misplaced_primary_is_refused_without_changing_the_image() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let image = scratch_dir("label-misplaced").join("btrfs-default.img");
    std::fs::copy(fixture("btrfs-default.img"), &image).expect("copy the fixture");
    let expected = label_of(&dump_super(&image));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image)
        .unwrap();
    let mut primary = [0u8; 4096];
    let mut mirror = [0u8; 4096];
    file.seek(SeekFrom::Start(64 << 10)).unwrap();
    file.read_exact(&mut primary).unwrap();
    file.seek(SeekFrom::Start(64 << 20)).unwrap();
    file.read_exact(&mut mirror).unwrap();
    file.seek(SeekFrom::Start(64 << 10)).unwrap();
    file.write_all(&mirror).unwrap();
    file.sync_all().unwrap();
    let before = std::fs::read(&image).unwrap();
    let out = tool("fs.btrfs")
        .arg(&image)
        .args(["set", "label", "MISPLACED"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a misplaced primary was accepted: {out:?}"
    );
    assert!(
        std::fs::read(&image).unwrap() == before,
        "refusal changed the image"
    );
    // Restore only the deliberately damaged input; independent readers must
    // still see the original volume and label after the refused operation.
    file.seek(SeekFrom::Start(64 << 10)).unwrap();
    file.write_all(&primary).unwrap();
    file.sync_all().unwrap();
    assert_eq!(label_of(&dump_super(&image)), expected);
    assert_btrfs_check_clean(&image, "after a refused misplaced-primary label write");
    let kernel = guest_kernel_read_ok(
        image.to_str().unwrap(),
        "the unchanged original label",
        "btrfs filesystem label \"$MNT\"\n",
    );
    assert_eq!(kernel.trim(), expected);
}
