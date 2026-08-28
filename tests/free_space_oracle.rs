//! What is free, worked out twice and required to agree.
//!
//! Btrfs records allocation from both ends. The extent tree holds one
//! item per allocated run; the free-space tree holds the complement,
//! maintained separately by the kernel. Deriving free space from the
//! first and comparing it against the second is a check neither source
//! can make of itself — they are different items, written at different
//! times, by different code.
//!
//! It is also the check that catches the mistake this is most likely to
//! make. Under `SKINNY_METADATA` a tree block is recorded as a
//! `METADATA_ITEM` whose key offset is the block's LEVEL, not a length.
//! Read as a length it gives extents of 0, 1 and 2 bytes, and every tree
//! block on the filesystem reads as free — an allocator would then hand
//! out the address of the root tree. Against the kernel's own free-space
//! tree that is not a subtle discrepancy; it is thousands of them.
//!
//! Fixtures are gitignored. Build them with `chore fixtures`.

use fs_btrfs::block_group::{BlockGroup, FreeExtent};
use fs_btrfs::fs::Filesystem;
use fs_core::FileDevice;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn images() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(share()) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "img"))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("btrfs-"))
        })
        .collect();
    out.sort();
    out
}

fn open(img: &Path) -> Option<Filesystem> {
    let dev = Arc::new(FileDevice::open(img).ok()?);
    Filesystem::mount(dev).ok()
}

/// A short description of where two free-space lists first diverge.
///
/// Printing the lists themselves is useless — a metadata group has
/// hundreds of runs. The first disagreement is the whole diagnosis.
fn first_difference(ours: &[FreeExtent], theirs: &[FreeExtent]) -> Option<String> {
    for (i, (a, b)) in ours.iter().zip(theirs.iter()).enumerate() {
        if a != b {
            return Some(format!(
                "run {i}: the extent tree says {}..{} ({} bytes), the free-space tree says \
                 {}..{} ({} bytes)",
                a.start,
                a.end(),
                a.len,
                b.start,
                b.end(),
                b.len
            ));
        }
    }
    if ours.len() != theirs.len() {
        let (longer, which) = if ours.len() > theirs.len() {
            (&ours[theirs.len()], "extent tree")
        } else {
            (&theirs[ours.len()], "free-space tree")
        };
        return Some(format!(
            "the lists agree for {} runs, then the {which} has {}..{} and the other has \
             nothing ({} runs vs {})",
            ours.len().min(theirs.len()),
            longer.start,
            longer.end(),
            ours.len(),
            theirs.len()
        ));
    }
    None
}

/// The two records of allocation describe the same filesystem.
#[test]
fn free_space_derived_from_the_extent_tree_matches_the_kernels_cache() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures; build them with `chore fixtures`");
        return;
    }

    let mut groups_checked = 0usize;
    let mut images_with_cache = 0usize;
    let mut runs = 0usize;

    for img in &images {
        let Some(fs) = open(img) else { continue };
        let name = img.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(groups) = fs.block_groups() else {
            continue;
        };
        assert!(
            !groups.is_empty(),
            "{name}: no block group at all — the filesystem's own root tree has to live \
             somewhere, so this is a read failure, not an empty filesystem"
        );

        let mut had_cache = false;
        // One traversal for every group, not one per group.
        let derived = fs
            .free_extents_by_group(&groups)
            .unwrap_or_else(|e| panic!("{name}: deriving free space: {e}"));
        for (group, ours) in groups.iter().zip(derived) {
            let theirs = match fs.cached_free_extents(group) {
                Ok(Some(t)) => t,
                // No free-space tree on this filesystem: nothing to
                // compare against, and not a failure.
                Ok(None) => continue,
                Err(e) => panic!(
                    "{name}: reading the free-space tree at {}: {e}",
                    group.start
                ),
            };
            had_cache = true;

            if let Some(diff) = first_difference(&ours, &theirs) {
                panic!(
                    "{name}: block group at {} (flags {:#x}, {} of {} bytes used) — the two \
                     records of what is free disagree. {diff}",
                    group.start, group.flags, group.used, group.length
                );
            }
            groups_checked += 1;
            runs += ours.len();
        }
        if had_cache {
            images_with_cache += 1;
        }
    }

    assert!(
        images_with_cache > 0,
        "not one of {} fixtures had a free-space tree, so the comparison never happened. \
         mkfs.btrfs has enabled it by default for years, so this is a fixture problem.",
        images.len()
    );
    eprintln!(
        "{groups_checked} block groups across {images_with_cache} images: {runs} free runs, \
         derived from the extent tree and confirmed against the kernel's free-space tree"
    );
}

/// Each group's `used` is what its allocated extents actually occupy.
///
/// A separate claim from the one above and a stricter one in a
/// particular way: free-space lists could agree while both were shifted,
/// but the used total is an independent number the kernel wrote into the
/// block group item.
#[test]
fn each_groups_used_count_matches_what_is_allocated_in_it() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }

    let mut checked = 0usize;
    for img in &images {
        let Some(fs) = open(img) else { continue };
        let name = img.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(groups) = fs.block_groups() else {
            continue;
        };

        for group in &groups {
            let Ok(free) = fs.free_extents(group) else {
                continue;
            };
            let free_bytes: u64 = free.iter().map(|r| r.len).sum();
            let allocated = group.length - free_bytes;

            assert_eq!(
                allocated, group.used,
                "{name}: the block group at {} says {} bytes are used, but the extent tree \
                 accounts for {allocated}. A difference of one nodesize means a metadata \
                 item was read as an extent length, or not read at all.",
                group.start, group.used
            );
            checked += 1;
        }
    }

    assert!(checked > 0, "no block group was checked");
    eprintln!("{checked} block groups account for exactly the bytes they say are used");
}

/// The superblock's `bytes_used` is the sum over every group.
///
/// The superblock writer takes this number on trust from its caller, so
/// this is where the rule it documents gets checked.
#[test]
fn the_superblock_total_is_the_sum_of_every_group() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }

    let mut checked = 0usize;
    for img in &images {
        let Some(fs) = open(img) else { continue };
        let name = img.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(groups) = fs.block_groups() else {
            continue;
        };
        let total: u64 = groups.iter().map(|g| g.used).sum();
        assert_eq!(
            total,
            fs.superblock().bytes_used,
            "{name}: the superblock says {} bytes are used and the {} block groups sum to \
             {total}",
            fs.superblock().bytes_used,
            groups.len()
        );
        checked += 1;
    }
    assert!(checked > 0, "no image was checked");
    eprintln!("{checked} superblocks agree with the sum of their block groups");
}

/// The address the allocator picks is genuinely free and correctly
/// aligned.
///
/// Checked against the kernel's free-space tree rather than against the
/// derivation that produced it, so a wrong derivation cannot certify its
/// own answer.
#[test]
fn the_address_the_allocator_picks_is_free_in_the_kernels_own_record() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }

    let mut checked = 0usize;
    for img in &images {
        let Some(fs) = open(img) else { continue };
        let name = img.file_name().unwrap().to_string_lossy().into_owned();
        let nodesize = fs.superblock().nodesize as u64;

        let Ok(at) = fs.find_metadata_block() else {
            continue;
        };

        assert_eq!(
            at % nodesize,
            0,
            "{name}: {at} is not aligned to a {nodesize}-byte tree block"
        );

        let Ok(groups) = fs.block_groups() else {
            continue;
        };
        let group: &BlockGroup = groups
            .iter()
            .find(|g| at >= g.start && at < g.end())
            .unwrap_or_else(|| panic!("{name}: {at} is in no block group at all"));

        assert!(
            group.holds_metadata(),
            "{name}: {at} is in the group at {} which has flags {:#x} and does not take \
             metadata",
            group.start,
            group.flags
        );

        let Ok(Some(cached)) = fs.cached_free_extents(group) else {
            continue;
        };
        let covered = cached
            .iter()
            .any(|r| at >= r.start && at + nodesize <= r.end());
        assert!(
            covered,
            "{name}: the allocator picked {at}, but the kernel's free-space tree does not \
             show {nodesize} free bytes there. Writing a tree block at an allocated \
             address is the failure this test exists to prevent."
        );
        checked += 1;
    }

    assert!(
        checked > 0,
        "the allocator found nowhere to put a block on any fixture"
    );
    eprintln!("{checked} allocations land on space the kernel also considers free");
}
