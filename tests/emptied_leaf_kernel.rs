//! A transaction whose edits leave a tree leaf with no items removes the
//! leaf from its tree (#546, part of #444).
//!
//! # What this covers
//!
//! The first shape: the emptied leaf is not its parent's first child, so
//! the parent's first key, and every key pointer above it, stays as it
//! is. The leaf's pointer leaves the parent node and its block is freed
//! as any relocated tree block is. Unlinking files one at a time is the
//! simplest way to get there: each unlink is one transaction removing one
//! file's items, and the leaf holding only the last files' inode items
//! empties on the last of them.
//!
//! # The oracles
//!
//! The volume is made by `mkfs.btrfs` and filled by the kernel, in the
//! harness VM. After this crate unlinks the files, `btrfs check` inspects
//! the whole volume (a pointer to a freed leaf, an empty leaf, or a leaf
//! the extent tree still records is reported there), `dump-tree` shows
//! the fs tree's leaves, and the kernel mounts the volume and lists the
//! directory.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::PathBuf;
use std::sync::Arc;

/// Files the kernel makes, empty, so each holds only its own items.
const FILES: usize = 200;
/// The files unlinked: the second half, whose inode items come last in
/// the fs tree.
const KEPT: usize = 100;

/// A scratch volume, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn name(i: usize) -> String {
    format!("f{i:03}")
}

/// The fs tree's leaves, in key order: each leaf's address, item count
/// and the objectid of every item in it.
fn fs_leaves(image: &std::path::Path) -> Vec<(u64, usize, Vec<u64>)> {
    let tree = dump_tree(image, "5");
    let mut out: Vec<(u64, usize, Vec<u64>)> = Vec::new();
    for line in tree.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() == Some(&"leaf") {
            let at = words[1].parse().unwrap();
            let items = words
                .iter()
                .position(|w| *w == "items")
                .and_then(|i| words.get(i + 1)?.parse().ok())
                .unwrap_or_else(|| panic!("no item count in {line:?}"));
            out.push((at, items, Vec::new()));
        } else if words.first() == Some(&"item") && words.get(2) == Some(&"key") {
            let objectid = words[3]
                .trim_start_matches('(')
                .parse()
                .unwrap_or_else(|_| panic!("no objectid in {line:?}"));
            out.last_mut()
                .unwrap_or_else(|| panic!("an item before any leaf: {line:?}"))
                .2
                .push(objectid);
        }
    }
    out
}

/// The kernel fills a directory, this driver unlinks the files whose
/// inode items fill a leaf of their own one at a time, and the last of
/// them empties that leaf. Then:
///
/// - `btrfs check` calls the volume clean and a kernel scrub passes;
/// - `dump-tree` shows no empty leaf, and fewer leaves than before;
/// - the kernel lists exactly the files left.
#[test]
fn a_leaf_its_edits_empty_leaves_its_tree_and_the_checker_and_kernel_agree() {
    let dir = PathBuf::from(temp_path!("emptied-leaf"));
    std::fs::create_dir_all(&dir).unwrap();
    let scratch = Scratch(dir);
    let image = scratch.image();
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .unwrap();
    // 4 KiB nodes, so a couple of hundred files take a tree of many
    // leaves under one root node.
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "4096", "-O", "no-holes"])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "mkfs.btrfs: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "making the files",
        &format!(
            "i=0\nwhile [ $i -lt {FILES} ]; do : > \"$MNT/$(printf f%03d $i)\"; i=$((i+1)); \
             done\nsync"
        ),
    );

    let dev = Arc::new(FileDevice::open_rw(&image).expect("open read-write"));
    let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write");
    let inos: Vec<u64> = (0..FILES)
        .map(|i| {
            fs.lookup_path(&format!("/{}", name(i)))
                .unwrap_or_else(|e| panic!("{}: {e}", name(i)))
                .ino
        })
        .collect();
    let gone =
        inos[KEPT..].iter().copied().min().unwrap()..=inos[KEPT..].iter().copied().max().unwrap();

    // Without a leaf of the unlinked files' items alone, below the
    // first, there is nothing for the unlinks to empty.
    let before = fs_leaves(&image);
    assert!(
        before
            .iter()
            .skip(1)
            .any(|(_, _, ids)| !ids.is_empty() && ids.iter().all(|id| gone.contains(id))),
        "no fs tree leaf past the first holds only the unlinked files' items, so this tests \
         nothing:\n{before:?}"
    );

    for i in KEPT..FILES {
        fs.unlink(256, name(i).as_bytes())
            .unwrap_or_else(|e| panic!("unlinking {}: {e}", name(i)));
    }
    drop(fs);

    assert_btrfs_check_clean(&image, "after the unlinks");
    let after = fs_leaves(&image);
    assert!(
        after.iter().all(|(_, items, _)| *items > 0),
        "the fs tree keeps an empty leaf:\n{after:?}"
    );
    assert!(
        after.len() < before.len(),
        "the fs tree has {} leaves after the unlinks and {} before: no leaf left it",
        after.len(),
        before.len()
    );
    assert!(
        after
            .iter()
            .all(|(_, _, ids)| ids.iter().all(|id| !gone.contains(id))),
        "the fs tree keeps items of an unlinked file:\n{after:?}"
    );

    let listed = guest_kernel_read_ok(&image.to_string_lossy(), "listing", "ls \"$MNT\"");
    let mut listed: Vec<&str> = listed.split_whitespace().collect();
    listed.sort_unstable();
    let want: Vec<String> = (0..KEPT).map(name).collect();
    assert_eq!(listed, want, "the kernel lists other files than were kept");
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}
