//! Moving a tree leaf that holds file data by full back reference keeps
//! those references true (#287).
//!
//! Balance relocates file data through a relocation tree, and the fs-tree
//! leaves it rewrites on the way come out flagged `FULL_BACKREF`: the data
//! extents they point at are recorded by `SHARED_DATA_REF`s naming the
//! LEAF'S ADDRESS as their parent, not by the tree, inode and offset that
//! an ordinary `EXTENT_DATA_REF` carries. A copy-on-write move gives the
//! leaf a new address, so every one of those references names a block
//! that is no longer there unless the move re-points it. `btrfs check`
//! then reports, for each extent, a "referencer count mismatch" for the
//! reference it found and for the one the extent tree holds, and a
//! "backpointer mismatch".
//!
//! `tests/block_group_used.rs` met this by chance, on whichever leaf its
//! search for a cross-group move happened to pick. This test picks such a
//! leaf on purpose: btrfs-progs' own dump of the extent tree names the
//! parents of the shared data references, and one of them that is an
//! fs-tree leaf is moved. Where the free space lies does not matter here.
//!
//! THE VOLUME IS MADE BY THE KERNEL, in the harness guest, the way
//! `block_group_used` makes its own: 4 KiB nodes, mixed block groups, a
//! full balance after the files are written (which is what leaves the
//! shared data references), and every other one of the lowest files
//! deleted, so the extent-tree leaves where the allocator lands have room
//! for the records a move adds.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::Commit;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The leaves of the fs tree, from btrfs-progs.
fn fs_tree_leaves(image: &Path) -> BTreeSet<u64> {
    dump_tree(image, "5")
        .lines()
        .filter_map(|l| l.strip_prefix("leaf "))
        .filter_map(|l| l.split_whitespace().next()?.parse::<u64>().ok())
        .collect()
}

/// Every parent a `SHARED_DATA_REF` names, from btrfs-progs' dump of the
/// extent tree: "shared data backref parent P count C".
fn shared_data_parents(image: &Path) -> BTreeSet<u64> {
    dump_tree(image, "extent")
        .lines()
        .filter_map(|l| l.split("shared data backref parent ").nth(1))
        .filter_map(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .collect()
}

#[test]
fn moving_a_leaf_that_holds_data_by_full_back_reference_keeps_the_volume_consistent() {
    let dir = PathBuf::from(temp_path!("full-backref-move"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(2 << 30))
        .unwrap();
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-M", "-n", "4096", "-s", "4096"])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    // Zeros: never read, and the copy back to the host is sparse.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "files relocated by a balance",
        "btrfs balance start --full-balance \"$MNT\" >/dev/null\n\
         for f in $(seq 0 399); do head -c 1048576 /dev/zero > \"$MNT/f$f\"; done\n\
         sync\n\
         btrfs balance start --full-balance \"$MNT\" >/dev/null\n\
         for f in $(seq 0 399); do\n\
           at=$(filefrag -v \"$MNT/f$f\" | awk '$1 == \"0:\" {sub(/\\.\\./, \"\", $4); print $4; exit}')\n\
           echo \"$at f$f\"\n\
         done | sort -n | head -n 128 | awk 'NR % 2 == 1' | while read -r _ f; do rm \"$MNT/$f\"; done\n\
         sync",
    );

    // The case itself, named by btrfs-progs rather than by this driver:
    // fs-tree leaves that data extents name as their parent.
    let leaves = fs_tree_leaves(&image);
    let parents = shared_data_parents(&image);
    let candidates: Vec<u64> = parents.intersection(&leaves).copied().collect();
    assert!(
        !candidates.is_empty(),
        "the balance left no fs-tree leaf that data extents name as their parent \
         ({} shared data parents, {} fs-tree leaves), so the volume no longer holds the case",
        parents.len(),
        leaves.len()
    );

    let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let generation = fs.superblock().generation + 1;

    // Any one of them will do. A plan that needs an insert into a full
    // extent-tree leaf, or touches a group whose free-space records
    // straddle a leaf (#177), is refused by name for reasons of its own
    // and is passed over; anything else is this test's failure.
    let mut chosen = None;
    let mut passed_over = Vec::new();
    for &leaf in &candidates {
        let plan = fs
            .plan_transaction_closed(&[leaf], 64)
            .expect("planning the transaction");
        match fs.render_plan(&plan, generation) {
            Ok(blocks) => {
                chosen = Some((leaf, plan, blocks));
                break;
            }
            Err(e)
                if e.to_string().contains("does not fit")
                    || e.to_string()
                        .contains("free-space records of the block group") =>
            {
                passed_over.push(format!("{leaf}: {e}"));
            }
            Err(e) => panic!("rendering a plan moving the leaf at {leaf}: {e}"),
        }
    }
    let (leaf, plan, blocks) = chosen.unwrap_or_else(|| {
        panic!(
            "every one of the {} fs-tree leaves holding data by full back reference was \
             passed over: {passed_over:#?}",
            candidates.len()
        )
    });
    let root = fs
        .planned_root(&plan)
        .expect("the plan moves the root tree");
    fs.commit(
        &blocks,
        &Commit {
            generation,
            root,
            invalidate_free_space_tree: false,
            ..Default::default()
        },
    )
    .expect("committing");
    drop(fs);
    println!(
        "[full-backref] moved the leaf at {leaf} and {} other blocks",
        plan.rewrites.len() - 1
    );

    assert_btrfs_check_clean(&image, "moving a full-backref leaf");
    let _ = std::fs::remove_dir_all(&dir);
}
