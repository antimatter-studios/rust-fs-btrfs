//! A transaction that allocates in one block group and releases in another
//! keeps each group's `used` count true (#223).
//!
//! A plan moves every block it rewrites: a release at the old address, an
//! allocation at the new one. The allocator takes the lowest free address
//! in the first group that holds metadata, so on a volume whose tree blocks
//! are spread over several groups the two ends of a move are routinely in
//! different groups. `bytes_used` still adds up — one allocation per
//! release — but each group's `BLOCK_GROUP_ITEM.used` did not move, and
//! `btrfs check` reports "block group [...] used X but extent items used Y".
//!
//! THE VOLUME IS MADE BY THE KERNEL, in the harness guest: 4 KiB nodes and
//! mixed block groups, so metadata is allocated from the same groups data
//! fills and tree blocks end up in every one of them. `btrfs balance`
//! first relocates mkfs's small first group, which would otherwise be
//! turned into free-space bitmaps (refused for its own reason). Nothing is
//! deleted, so every group's free-space records stay in one leaf, which the
//! test asserts from btrfs-progs' own dump before relying on it.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::Commit;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The leaves of the fs and csum trees, from btrfs-progs.
fn tree_leaves(image: &Path) -> Vec<u64> {
    ["5", "7"]
        .iter()
        .flat_map(|tree| {
            dump_tree(image, tree)
                .lines()
                .filter_map(|l| l.strip_prefix("leaf "))
                .filter_map(|l| l.split_whitespace().next()?.parse::<u64>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn a_transaction_across_block_groups_keeps_each_groups_used_count_true() {
    let dir = PathBuf::from(temp_path!("bg-used"));
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
        "several groups of files",
        "btrfs balance start --full-balance \"$MNT\" >/dev/null\n\
         for f in $(seq 0 599); do head -c 1048576 /dev/zero > \"$MNT/f$f\"; done\n\
         sync",
    );

    let fst_leaves = dump_tree(&image, "10")
        .lines()
        .filter(|l| l.starts_with("leaf "))
        .count();
    assert_eq!(
        fst_leaves, 1,
        "the free-space tree spans {fst_leaves} leaves, so a group's records may straddle \
         one (#177) and btrfs check would fail for that instead"
    );

    // A tree leaf in the highest group that holds one: the allocator
    // fills the lowest group first, so moving it crosses groups.
    let groups = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        fs.block_groups().unwrap()
    };
    let dirty = tree_leaves(&image)
        .into_iter()
        .max()
        .expect("the fs and csum trees have leaves");
    let group_of = |a: u64| groups.iter().find(|g| g.contains(a)).map(|g| g.start);

    let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let generation = fs.superblock().generation + 1;
    let plan = fs
        .plan_transaction_closed(&[dirty], 64)
        .expect("planning the transaction");
    let released: BTreeSet<_> = plan.released().into_iter().map(group_of).collect();
    let allocated: BTreeSet<_> = plan.allocated().into_iter().map(group_of).collect();
    assert_ne!(
        released, allocated,
        "the plan releases and allocates in the same groups, so it tests nothing"
    );
    let blocks = fs.render_plan(&plan, generation).expect("rendering");
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
        "[bg-used] moved {} blocks: released in groups {released:?}, allocated in {allocated:?}",
        plan.rewrites.len()
    );

    assert_btrfs_check_clean(&image, "a transaction across block groups");
    let _ = std::fs::remove_dir_all(&dir);
}
