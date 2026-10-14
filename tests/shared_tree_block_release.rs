//! A transaction preserves a tree block a snapshot still shares
//! (#178).
//!
//! `apply_records` released each block a plan rewrites by deleting its
//! `METADATA_ITEM`, without reading its reference count. After a snapshot
//! of a tree whose root is a node, the blocks under the root are referenced
//! by both trees. Rewriting one deleted its record outright, so the
//! snapshot's tree pointed at a block the allocator then considered free.
//! Dropping one reference, and adding explicit ones for a shared block's
//! children when it's copied, is what the kernel does. The plan now keeps
//! the old block and renders that reference bookkeeping (#262).
//!
//! The image is `snapshot/btrfs-nodatacow-snapshot.img` from `chore
//! fixtures`, a read-only snapshot the kernel took of the top-level
//! subvolume. btrfs-progs — in the harness VM, where the tools are —
//! names a tree block the extent tree records with two references, and
//! the planner and renderer are asked to move it.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, fixture, guest_kernel_read_ok, sha256_hex, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A tree block the extent tree records with more than one reference.
fn shared_tree_block(image: &Path) -> u64 {
    let dump = dump_tree(image, "extent");
    let lines: Vec<&str> = dump.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let Some(key) = line.split("key (").nth(1) else {
            continue;
        };
        let mut words = key.split_whitespace();
        let (Some(bytenr), Some("METADATA_ITEM")) = (words.next(), words.next()) else {
            continue;
        };
        let refs = lines
            .get(i + 1)
            .and_then(|l| l.split_whitespace().skip_while(|w| *w != "refs").nth(1))
            .and_then(|r| r.parse::<u64>().ok());
        if refs.is_some_and(|r| r > 1) {
            return bytenr.parse().unwrap();
        }
    }
    panic!("the fixture holds no tree block with more than one reference, so it no longer holds the case");
}

fn extent_record(dump: &str, bytenr: u64) -> String {
    let lines: Vec<_> = dump.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.contains(&format!("key ({bytenr} EXTENT_ITEM ")))
        .expect("the data extent's record");
    lines[at + 1..]
        .iter()
        .take_while(|line| !line.contains("key ("))
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_plan_that_releases_a_shared_tree_block_is_refused() {
    let image = fixture("snapshot/btrfs-nodatacow-snapshot.img");
    let shared = shared_tree_block(&image);
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).expect("mount");
    let generation = fs.superblock().generation + 1;
    let rendered = fs.plan_transaction_closed(&[shared], 8).and_then(|plan| {
        assert!(
            plan.kept.contains(&shared) && !plan.released().contains(&shared),
            "the plan must move the shared block while keeping its old address"
        );
        fs.render_plan(&plan, generation)
    });
    assert!(
        rendered.is_ok(),
        "snapshot-aware reference bookkeeping failed: {rendered:?}"
    );
}

#[test]
fn snapshot_data_survives_truncate_unlink_and_replacing_rename() {
    for operation in ["truncate", "unlink", "rename"] {
        let image = PathBuf::from(temp_path!("snapshot-unref-{operation}.img"));
        std::fs::copy(fixture("snapshot/btrfs-nodatacow-snapshot.img"), &image).unwrap();
        let original;
        let bytenr;
        {
            let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
            let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap();
            let ino = fs.lookup_path("/nc/inplace.bin").unwrap().ino;
            let parent = fs.lookup_path("/nc").unwrap().ino;
            original = fs.read_file(ino).unwrap();
            assert!(!original.is_empty(), "fixture must hold external data");
            let tree = dump_tree(&image, "5");
            let file = tree
                .split(&format!("key ({ino} EXTENT_DATA 0)"))
                .nth(1)
                .expect("the file's first extent");
            bytenr = file
                .lines()
                .find(|line| line.contains("disk byte"))
                .unwrap()
                .split("disk byte ")
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            let before = extent_record(&dump_tree(&image, "extent"), bytenr);
            assert!(
                before.contains("refs 1 "),
                "fixture no longer has implicitly shared data: {before}"
            );
            assert!(
                before.contains("extent data backref"),
                "missing original keyed reference: {before}"
            );
            match operation {
                "truncate" => fs.truncate(ino, 0).unwrap(),
                "unlink" => fs.unlink(parent, b"inplace.bin").unwrap(),
                "rename" => {
                    fs.create(parent, b"replacement", 0o644, 0, 0).unwrap();
                    fs.rename(parent, b"replacement", parent, b"inplace.bin")
                        .unwrap();
                }
                _ => unreachable!(),
            }
        }
        assert_btrfs_check_clean(&image, operation);
        let extents = extent_record(&dump_tree(&image, "extent"), bytenr);
        assert!(
            extents.contains("shared data backref"),
            "no explicit snapshot data reference after {operation}: {extents}"
        );
        assert!(
            extents.contains("refs 1 "),
            "snapshot should retain exactly one data reference: {extents}"
        );
        assert!(
            !extents.contains("extent data backref"),
            "removed file's keyed reference remains: {extents}"
        );
        let live = if operation == "unlink" {
            "test ! -e nc/inplace.bin"
        } else {
            "test -f nc/inplace.bin; test ! -s nc/inplace.bin"
        };
        let seen = guest_kernel_read_ok(
            &image.to_string_lossy(),
            operation,
            &format!("cd \"$MNT\"\nsha256sum snap/nc/inplace.bin | cut -d' ' -f1\n{live}"),
        );
        assert_eq!(
            seen.trim(),
            sha256_hex(&original),
            "snapshot changed after {operation}"
        );
    }
}
