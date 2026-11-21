//! Salvage finds tree roots as btrfs-progs' recovery tools find them.
//!
//! In the harness VM: `btrfs restore -l [-t BYTENR]` lists the tree roots a
//! root tree records, the current one or an older one; `btrfs-find-root
//! [-o OWNER]` scans the metadata chunks for intact blocks a tree owns.
//! The salvage root items, root tree candidates and raw scan have to name
//! the same blocks, including on a copy whose current root tree is zeroed,
//! where `restore -l` fails and an older root still leads somewhere.

use fs_btrfs::salvage::{
    best_copy, build_chunk_map, metadata_ranges, read_root_items, root_tree_candidates,
    scan_tree_blocks, superblock_copies, Layout, RootItem, RootPointer, RootSource,
};
use fs_btrfs_test_support::{copy_sparse, fixture, oracle, temp_path};
use fs_core::FileDevice;
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::ops::ControlFlow;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

fn numbers_after(line: &str, key: &str) -> Option<u64> {
    let mut words = line.split_whitespace();
    while let Some(word) = words.next() {
        if word == key {
            return words.next()?.trim_end_matches(')').parse().ok();
        }
    }
    None
}

/// `(bytenr, level)` of every tree `btrfs restore -l` lists, and whether it
/// ran cleanly.
fn restore_list(image: &Path, tree_root: Option<u64>) -> (bool, BTreeSet<(u64, u8)>, String) {
    let mut tool = oracle("btrfs").args(["restore", "-l"]);
    if let Some(bytenr) = tree_root {
        tool = tool.args(["-t", &bytenr.to_string()]);
    }
    let out = tool.arg(image).output();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let roots = text
        .lines()
        .filter(|line| line.trim_start().starts_with("tree key ("))
        .map(|line| {
            let after = line.split_once(')').unwrap().1;
            let mut words = after.split_whitespace();
            let bytenr = words.next().unwrap().parse().unwrap();
            let level = numbers_after(after, "level").unwrap() as u8;
            (bytenr, level)
        })
        .collect();
    (out.status.success(), roots, text)
}

/// `(bytenr, generation, level)` of every block `btrfs-find-root` reports.
fn find_root(image: &Path, owner: u64) -> BTreeSet<(u64, u64, u8)> {
    let out = oracle("btrfs-find-root")
        .args(["-o", &owner.to_string()])
        .arg(image)
        .output();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "btrfs-find-root -o {owner}:\n{text}");
    let mut found = BTreeSet::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Found tree root at ") {
            let words: Vec<&str> = rest.split_whitespace().collect();
            found.insert((
                words[0].parse().unwrap(),
                numbers_after(rest, "gen").unwrap(),
                numbers_after(rest, "level").unwrap() as u8,
            ));
        } else if let Some(rest) = line.strip_prefix("Well block ") {
            let (bytenr, rest) = rest.split_once('(').unwrap();
            let generation = rest.split_once("gen: ").unwrap().1;
            let (generation, rest) = generation.split_once(' ').unwrap();
            let level = rest.split_once("level: ").unwrap().1;
            let level: String = level.chars().take_while(char::is_ascii_digit).collect();
            found.insert((
                bytenr.parse().unwrap(),
                generation.parse().unwrap(),
                level.parse().unwrap(),
            ));
        }
    }
    found
}

fn open(image: &Path) -> (FileDevice, Layout, Vec<fs_btrfs::salvage::SuperblockCopy>) {
    let device = FileDevice::open(image).expect("open the image");
    let copies = superblock_copies(&device);
    let sb = best_copy(&copies).unwrap().superblock.clone().unwrap();
    let scan = build_chunk_map(&device, &sb);
    (device, Layout::new(&sb, scan.map), copies)
}

fn roots_of(items: &[RootItem]) -> BTreeSet<(u64, u8)> {
    items.iter().map(|r| (r.bytenr, r.level)).collect()
}

/// The current root tree's items are the trees `btrfs restore -l` lists,
/// on a plain volume and on one with subvolumes and snapshots.
#[test]
fn the_current_root_tree_lists_what_restore_lists() {
    for name in ["btrfs-rich.img", "btrfs-subvol.img"] {
        let image = fixture(name);
        let (device, layout, copies) = open(&image);
        let candidates = root_tree_candidates(&copies);
        let current = &candidates[0];
        assert!(current
            .sources
            .contains(&RootSource::Superblock { mirror: 0 }));
        let (items, walk) = read_root_items(&device, &layout, current.tree_root);
        assert!(walk.damaged.is_empty(), "{name}: {:?}", walk.damaged);
        let (ok, listed, text) = restore_list(&image, None);
        assert!(ok, "{name}: restore -l:\n{text}");
        assert!(!listed.is_empty(), "{name}: restore listed trees");
        assert_eq!(roots_of(&items), listed, "{name}");
    }
}

/// Every root tree candidate, current and from the backup ring, newest
/// first, lists what `restore -l -t` lists for it, when restore can read
/// it; where restore cannot, the walk reports damage.
#[test]
fn every_backup_root_tree_lists_what_restore_lists_for_it() {
    let image = fixture("btrfs-rich.img");
    let (device, layout, copies) = open(&image);
    let candidates = root_tree_candidates(&copies);
    assert!(
        candidates.len() > 1,
        "the backup ring names older root trees"
    );
    let generations: Vec<u64> = candidates
        .iter()
        .map(|c| c.tree_root.generation.unwrap())
        .collect();
    assert!(
        generations.windows(2).all(|w| w[0] > w[1]),
        "{generations:?}"
    );
    assert!(candidates.iter().skip(1).all(|c| c
        .sources
        .iter()
        .all(|s| matches!(s, RootSource::Backup { .. }))));

    let mut compared = 0;
    for candidate in &candidates {
        let (items, walk) = read_root_items(&device, &layout, candidate.tree_root);
        let (ok, listed, text) = restore_list(&image, Some(candidate.tree_root.bytenr));
        if ok && !listed.is_empty() && walk.damaged.is_empty() {
            assert_eq!(roots_of(&items), listed, "{candidate:?}:\n{text}");
            compared += 1;
        } else {
            assert!(
                !walk.damaged.is_empty() || !ok,
                "restore and salvage disagree on {candidate:?}:\n{text}"
            );
        }
    }
    assert!(compared >= 2, "at least two generations compared");
}

/// The raw scan finds the same intact root tree blocks, and filesystem
/// tree blocks, as `btrfs-find-root`.
#[test]
fn the_scan_finds_what_find_root_finds() {
    for name in ["btrfs-rich.img", "btrfs-dup.img"] {
        let image = fixture(name);
        let (device, layout, _) = open(&image);
        let mut found = Vec::new();
        for (offset, length) in metadata_ranges(&layout) {
            let summary = scan_tree_blocks(&device, &layout, offset, length, &mut |block| {
                found.push(block);
                ControlFlow::Continue(())
            });
            assert!(summary.unreadable.is_empty());
        }
        assert!(!found.is_empty());
        for owner in [1u64, 5] {
            let ours: BTreeSet<(u64, u64, u8)> = found
                .iter()
                .filter(|b| b.is_consistent() && b.header.owner == owner)
                .map(|b| (b.header.bytenr, b.header.generation, b.header.level))
                .collect();
            assert!(!ours.is_empty(), "{name}: owner {owner} has intact blocks");
            assert_eq!(ours, find_root(&image, owner), "{name}: owner {owner}");
        }
    }
}

/// The current root tree's block zeroed in every copy: `restore -l` cannot
/// read the volume, the current candidate's walk reports the damage, and
/// the newest backup root tree still lists what `restore -l -t` lists for
/// it.
#[test]
fn with_the_current_root_tree_zeroed_a_backup_root_tree_still_leads() {
    let dir = PathBuf::from(temp_path!("salvage-roots-zeroed"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("rich.img");
    copy_sparse(fixture("btrfs-rich.img"), &image).unwrap();

    let (_, layout, copies) = open(&image);
    let current: RootPointer = root_tree_candidates(&copies)[0].tree_root;
    let file = OpenOptions::new().write(true).open(&image).unwrap();
    let zeros = vec![0u8; layout.geometry.nodesize as usize];
    for mirror in 0..layout.copies(current.bytenr) {
        let at = layout
            .chunks
            .map_mirror(current.bytenr, mirror)
            .unwrap()
            .physical;
        file.write_all_at(&zeros, at).unwrap();
    }

    let (ok, listed, text) = restore_list(&image, None);
    assert!(
        !ok || listed.is_empty(),
        "restore read a zeroed root tree:\n{text}"
    );

    let (device, layout, copies) = open(&image);
    let candidates = root_tree_candidates(&copies);
    let (items, walk) = read_root_items(&device, &layout, candidates[0].tree_root);
    assert!(items.is_empty());
    assert_eq!(walk.damaged.len(), 1);

    let older = candidates
        .iter()
        .skip(1)
        .find(|c| {
            read_root_items(&device, &layout, c.tree_root)
                .1
                .damaged
                .is_empty()
        })
        .expect("an older root tree reads cleanly");
    let (items, _) = read_root_items(&device, &layout, older.tree_root);
    let (ok, listed, text) = restore_list(&image, Some(older.tree_root.bytenr));
    assert!(ok, "restore -t {}:\n{text}", older.tree_root.bytenr);
    assert_eq!(roots_of(&items), listed);
    assert!(
        items.iter().any(|r| r.objectid == 5),
        "the older root tree names the fs tree"
    );
}
