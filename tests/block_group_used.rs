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
//! turned into free-space bitmaps (refused for its own reason). A second
//! balance after the files are written packs every group, so no group
//! has free space to speak of — and then every other one of the 128
//! files at the LOWEST addresses, by `filefrag`, is deleted. That opens
//! 64 one-mebibyte holes at the bottom of the first group, which is where
//! the allocator looks first, and each is one free-space record.
//! Deleting the 64 lowest as one contiguous run was measured not to do:
//! every one of the 22 plans that crossed groups then needed an insert
//! into a full extent-tree leaf, the leaf an insert at the bottom of the
//! hole lands in having no room. Alternate files leave items, and room,
//! in every leaf over the holes. A plan touching a group whose records
//! straddle a leaf (#177, read from btrfs-progs' own dump), or one needing
//! an insert into a full extent-tree leaf, is still passed over rather
//! than used.
//!
//! WHICH BLOCK IS MOVED is worked out from the volume, not assumed. The
//! kernel decides where every tree block ends up, and on some runs it
//! packed every fs and csum leaf into the very group the allocator draws
//! from, so that no move of one could cross anything (#279, #280). The
//! test asks the allocator which group its next block is in, and tries
//! every tree block outside that group first, of every tree a plan can
//! move, leaves and nodes, with no cap. When none will do, the failure
//! names the layout — the target group, how the tree blocks are spread
//! over the groups, and why each crossing plan was passed over — so a
//! layout miss reads as one rather than as a fault in the driver.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::Commit;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The groups whose free-space records btrfs-progs shows in more than one leaf.
fn straddling_groups(image: &Path) -> BTreeSet<u64> {
    let dump = dump_tree(image, "10");
    let mut leaf = 0usize;
    // (start, length, first leaf, last leaf)
    let mut groups: Vec<(u64, u64, usize, usize)> = Vec::new();
    for line in dump.lines() {
        if line.starts_with("leaf ") {
            leaf += 1;
            continue;
        }
        let line = line.trim_start();
        if !line.starts_with("item ") {
            continue;
        }
        let Some(key) = line.split("key (").nth(1) else {
            continue;
        };
        let mut words = key.split_whitespace();
        let (Some(objectid), Some(kind), Some(offset)) = (
            words.next().and_then(|w| w.parse::<u64>().ok()),
            words.next(),
            words
                .next()
                .and_then(|w| w.trim_end_matches(')').parse::<u64>().ok()),
        ) else {
            continue;
        };
        match kind {
            "FREE_SPACE_INFO" => groups.push((objectid, offset, leaf, leaf)),
            "FREE_SPACE_EXTENT" | "FREE_SPACE_BITMAP" => {
                if let Some(g) = groups
                    .iter_mut()
                    .rev()
                    .find(|g| objectid >= g.0 && objectid < g.0 + g.1)
                {
                    g.3 = leaf;
                }
            }
            _ => {}
        }
    }
    groups
        .into_iter()
        .filter(|g| g.2 != g.3)
        .map(|g| g.0)
        .collect()
}

/// Every block of every tree a plan can move, leaves and nodes alike,
/// from btrfs-progs: the fs, csum, extent, root, dev and free-space trees.
///
/// Not only the fs and csum leaves, and not only the first few hundred.
/// Whether a move crosses groups depends on where the block sits relative
/// to where the allocator will put its copy, and a search confined to one
/// kind of block found none on volumes where the kernel had packed those
/// into the very group the allocator draws from (#279, #280).
fn tree_blocks(image: &Path) -> Vec<u64> {
    ["5", "7", "2", "1", "4", "10"]
        .iter()
        .flat_map(|tree| {
            dump_tree(image, tree)
                .lines()
                .filter_map(|l| l.strip_prefix("leaf ").or_else(|| l.strip_prefix("node ")))
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
         for f in $(seq 0 399); do head -c 1048576 /dev/zero > \"$MNT/f$f\"; done\n\
         sync\n\
         btrfs balance start --full-balance \"$MNT\" >/dev/null\n\
         for f in $(seq 0 399); do\n\
           at=$(filefrag -v \"$MNT/f$f\" | awk '$1 == \"0:\" {sub(/\\.\\./, \"\", $4); print $4; exit}')\n\
           echo \"$at f$f\"\n\
         done | sort -n | head -n 128 | awk 'NR % 2 == 1' | while read -r _ f; do rm \"$MNT/$f\"; done\n\
         sync",
    );

    // Which leaves each group's free-space records sit in, from
    // btrfs-progs. A plan touching a group whose records straddle a leaf
    // is a different defect (#177), so the plan chosen below touches none.
    let straddling = straddling_groups(&image);

    let groups = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        fs.block_groups().unwrap()
    };
    let group_of = |a: u64| groups.iter().find(|g| g.contains(a)).map(|g| g.start);

    let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let generation = fs.superblock().generation + 1;

    // The allocator takes the lowest free address, so every copy a plan
    // makes lands in one group, and a move crosses groups exactly when
    // some block it rewrites lies outside that group. Which group that
    // is comes from the allocator itself, and the blocks outside it are
    // tried first, every one of them if need be.
    let blocks = tree_blocks(&image);
    assert!(!blocks.is_empty(), "btrfs-progs listed no tree blocks");
    let target = {
        let plan = fs
            .plan_transaction_closed(&[blocks[0]], 64)
            .expect("planning the transaction");
        plan.allocated().into_iter().min().and_then(group_of)
    };
    let mut by_group: BTreeMap<Option<u64>, usize> = BTreeMap::new();
    for &b in &blocks {
        *by_group.entry(group_of(b)).or_default() += 1;
    }
    let mut candidates = blocks.clone();
    candidates.sort_by_key(|&b| group_of(b) == target);

    let mut chosen = None;
    let mut crossed = 0usize;
    let mut on_a_straddle = 0usize;
    let mut full_leaf = 0usize;
    let mut first_full = None;
    for &dirty in &candidates {
        let plan = fs
            .plan_transaction_closed(&[dirty], 64)
            .expect("planning the transaction");
        let released: BTreeSet<_> = plan.released().into_iter().map(group_of).collect();
        let allocated: BTreeSet<_> = plan.allocated().into_iter().map(group_of).collect();
        if released == allocated {
            continue;
        }
        crossed += 1;
        let touches_a_straddle = released
            .iter()
            .chain(allocated.iter())
            .any(|g| g.is_some_and(|g| straddling.contains(&g)));
        if touches_a_straddle {
            on_a_straddle += 1;
            continue;
        }
        // Inserting into a full extent-tree leaf is not implemented, and
        // is refused by name; a plan that needs it is not this test's.
        match fs.render_plan(&plan, generation) {
            Ok(rendered) => {
                chosen = Some((plan, rendered, released, allocated));
                break;
            }
            Err(e) if e.to_string().contains("does not fit") => {
                full_leaf += 1;
                first_full.get_or_insert(format!("moving the block at {dirty}: {e}"));
            }
            Err(e) => panic!("rendering a plan moving the block at {dirty}: {e}"),
        }
    }
    // NOT A SKIP, and not the driver's fault either: when nothing is
    // chosen, the message says what the kernel's layout was, so a layout
    // miss reads as one rather than as a regression.
    let (plan, rendered, released, allocated) = chosen.unwrap_or_else(|| {
        panic!(
            "the volume the kernel made holds no tree block whose move crosses block groups \
             cleanly, so the layout, not the driver, is what failed. The allocator's next \
             block is in the group at {target:?}; the {} tree blocks are spread over groups \
             {by_group:?}. Of them, {crossed} crossed groups: {on_a_straddle} touched one of \
             the {} groups whose free-space records straddle a leaf ({straddling:?}), and \
             {full_leaf} needed an insert into a full extent-tree leaf (first: {})",
            blocks.len(),
            straddling.len(),
            first_full.as_deref().unwrap_or("none")
        )
    });
    let root = fs
        .planned_root(&plan)
        .expect("the plan moves the root tree");
    fs.commit(
        &rendered,
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
