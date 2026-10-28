//! `fs.btrfs defragment` on a volume the kernel fragmented, judged by what
//! is not this crate (#305).
//!
//! The library's defragment is judged the same way in defrag_kernel.rs;
//! this is the same change reached through the tool, so it is the tool's
//! path, its target option and its refusal that are judged here. The
//! kernel appends to two files in turn, a sync between each append, so no
//! two of a file's extents are neighbours. `fs.btrfs defragment` then
//! rewrites one whole and the other to a target, and `btrfs check`
//! (with `--check-data-csum`), `dump-tree`'s count of `EXTENT_DATA` items,
//! `filefrag` and the bytes the kernel reads are the judges. Last, once the
//! kernel has snapshotted the volume, the tool refuses with exit 3 and
//! writes nothing.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Eight 64 KiB appends, then 12,345 bytes, so it ends partway through a
/// sector.
const FRAG: &str = "frag";
/// Eight 16 KiB appends, defragmented to a 32 KiB target.
const OTHER: &str = "other";

const FILES: [&str; 2] = [FRAG, OTHER];

const FILL: &str = "cd \"$MNT\"\n\
     for i in 1 2 3 4 5 6 7 8; do\n\
       head -c 65536 /dev/urandom >> frag\n\
       head -c 16384 /dev/urandom >> other\n\
       sync\n\
     done\n\
     head -c 12345 /dev/urandom >> frag\n\
     sync";

/// A scratch volume the kernel filled, removed when it drops — including
/// on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let scratch = Scratch(scratch_dir(tag));
        let image = scratch.image();
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
        guest_kernel_write_ok(&image.to_string_lossy(), tag, FILL);
        scratch
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

fn fs_btrfs(img: &Path) -> std::process::Command {
    let mut cmd = tool("fs.btrfs");
    cmd.arg(img);
    cmd
}

/// Each file's inode, size, SHA-256 and `filefrag`'s count of its
/// extents, as the kernel reports them.
fn kernel_view(image: &Path, what: &str) -> BTreeMap<String, (u64, u64, String, u64)> {
    let script: String = FILES
        .iter()
        .map(|name| {
            format!(
                "printf '%s %s %s %s %s\\n' {name} \"$(stat -c %i \"$MNT/{name}\")\" \
                 \"$(stat -c %s \"$MNT/{name}\")\" \
                 \"$(sha256sum \"$MNT/{name}\" | cut -d' ' -f1)\" \
                 \"$(filefrag \"$MNT/{name}\" | awk '{{print $2}}')\"\n"
            )
        })
        .collect();
    guest_kernel_read_ok(&image.to_string_lossy(), what, &script)
        .lines()
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(words.len(), 5, "[{what}] the kernel said {line:?}");
            let num = |w: &str| -> u64 { w.parse().unwrap_or_else(|_| panic!("{line:?}")) };
            (
                words[0].to_string(),
                (
                    num(words[1]),
                    num(words[2]),
                    words[3].to_string(),
                    num(words[4]),
                ),
            )
        })
        .collect()
}

/// How many `EXTENT_DATA` items `dump-tree` lists for inode `ino` in the
/// top-level subvolume.
fn dumped_items(image: &Path, ino: u64) -> usize {
    let needle = format!("key ({ino} EXTENT_DATA ");
    dump_tree(image, "5").matches(&needle).count()
}

#[test]
fn files_the_tool_defragments_are_judged_by_the_checker_and_the_kernel() {
    let scratch = Scratch::new("cli-defrag-kernel");
    let image = scratch.image();
    let before = kernel_view(&image, "before the defragment");
    assert_eq!(before.len(), FILES.len(), "{before:?}");
    for name in FILES {
        assert!(
            before[name].3 > 1,
            "the kernel made {name} as {} extent(s); the fill no longer fragments it",
            before[name].3
        );
    }
    let (frag, other) = (before[FRAG].0, before[OTHER].0);
    let dumped_frag = dumped_items(&image, frag);
    assert_eq!(dumped_items(&image, other), 8, "dump-tree: {OTHER} before");

    let report = stdout(&ok(fs_btrfs(&image).args(["defragment", "/frag"])));
    assert_eq!(
        json_field(&report, "items_before"),
        dumped_frag.to_string(),
        "the tool counts the items dump-tree lists: {report}"
    );
    assert_eq!(json_field(&report, "items_after"), "1", "{report}");
    assert_eq!(json_field(&report, "runs"), "1", "{report}");

    // Eight 16 KiB extents to runs of at most 32 KiB: four of two each.
    let report = stdout(&ok(fs_btrfs(&image).args([
        "defragment",
        "--target",
        "32K",
        "/other",
    ])));
    assert_eq!(json_field(&report, "runs"), "4", "{report}");
    assert_eq!(json_field(&report, "items_after"), "4", "{report}");

    assert_btrfs_check_clean(&image, "after fs.btrfs defragment");
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
    assert_eq!(dumped_items(&image, frag), 1, "dump-tree: {FRAG}");
    assert_eq!(dumped_items(&image, other), 4, "dump-tree: {OTHER}");

    let after = kernel_view(&image, "after the defragment");
    for name in FILES {
        let (b, a) = (&before[name], &after[name]);
        assert_eq!(
            (a.1, &a.2),
            (b.1, &b.2),
            "the kernel reads {name} differently after the defragment"
        );
    }
    assert_eq!(after[FRAG].3, 1, "filefrag: {FRAG}");
    assert!(
        after[OTHER].3 <= 4,
        "filefrag finds {} extents in {OTHER}, defragmented into four",
        after[OTHER].3
    );

    // A snapshot may be reading the extents: refused, and nothing written.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel snapshots the volume and fragments other again",
        "cd \"$MNT\"\n\
         btrfs subvolume snapshot \"$MNT\" \"$MNT/snap\" >/dev/null\n\
         sync\n\
         for i in 1 2; do\n\
           head -c 16384 /dev/urandom >> other\n\
           sync\n\
         done",
    );
    assert_btrfs_check_clean(&image, "after the kernel snapshotted the volume");
    let snapshotted = std::fs::read(&image).unwrap();
    let message = refused(fs_btrfs(&image).args(["defragment", "/other"]), 3);
    assert!(message.contains("snapshot"), "{message}");
    assert!(
        std::fs::read(&image).unwrap() == snapshotted,
        "a refused defragment wrote to the image"
    );
}
