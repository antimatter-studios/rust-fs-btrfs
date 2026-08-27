//! Every allocation record the kernel wrote, rebuilt from what a writer
//! would know.
//!
//! Handing out a free address is only half of allocating. Until the
//! extent tree says the address is taken, the next call returns it
//! again, and the second block lands on the first. The item that closes
//! that gap has a shape which depends on feature bits rather than on the
//! struct definition, so it is checked the same way the tree blocks are:
//! against the kernel's own.
//!
//! Each `METADATA_ITEM` on each fixture is rebuilt from only the four
//! things a writer has when it allocates — the address, the level, the
//! transaction, and the owning tree — and required to come back byte for
//! byte.
//!
//! Fixtures are gitignored. Build them with `chore fixtures`.

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::chunk::{key_type, DiskKey};
use fs_btrfs::extent_write::{
    offsets, record_tree_block, used_after_allocating, TreeBlockAllocation,
    SKINNY_METADATA_ITEM_SIZE,
};
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

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Rebuilding an allocation record gives back what the kernel wrote.
#[test]
fn every_metadata_item_re_encodes_identically() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures; build them with `chore fixtures`");
        return;
    }

    let mut total = 0usize;
    let mut images_checked = 0usize;

    for img in &images {
        let Some(fs) = open(img) else { continue };
        let name = img.file_name().unwrap().to_string_lossy().into_owned();
        let sb = fs.superblock().clone();

        let mut seen = 0usize;
        let mut failure: Option<String> = None;

        let walked = fs.for_each_extent_item(&mut |key: &DiskKey, data: &[u8]| {
            if key.key_type != key_type::METADATA_ITEM || failure.is_some() {
                return;
            }
            seen += 1;

            // Only what a writer would have: where the block went, how
            // tall it is, which transaction, and whose tree. Everything
            // else in the item has to come from the encoder, or the
            // comparison is circular.
            let alloc = TreeBlockAllocation {
                bytenr: key.objectid,
                level: key.offset as u8,
                generation: le64(data, offsets::GENERATION),
                owner: le64(data, offsets::REF_OFFSET),
            };

            let (ours_key, ours) = match record_tree_block(&sb, alloc) {
                Ok(v) => v,
                Err(e) => {
                    failure = Some(format!(
                        "the kernel wrote a METADATA_ITEM at {} and this refused to: {e}",
                        key.objectid
                    ));
                    return;
                }
            };

            if data.len() != SKINNY_METADATA_ITEM_SIZE {
                failure = Some(format!(
                    "the item at {} is {} bytes and this writes {SKINNY_METADATA_ITEM_SIZE}. \
                     A different length means a reference that is not inline, or a \
                     tree_block_info this does not write.",
                    key.objectid,
                    data.len()
                ));
                return;
            }

            if (ours_key.objectid, ours_key.key_type, ours_key.offset)
                != (key.objectid, key.key_type, key.offset)
            {
                failure = Some(format!(
                    "the key for the block at {} came back as {ours_key:?}, not {key:?}",
                    key.objectid
                ));
                return;
            }

            if let Some(i) = (0..SKINNY_METADATA_ITEM_SIZE).find(|&i| ours[i] != data[i]) {
                let field = match i {
                    0..=7 => "refs",
                    8..=15 => "generation",
                    16..=23 => "flags",
                    24 => "the inline reference's type",
                    _ => "the inline reference's offset",
                };
                failure = Some(format!(
                    "the item for the block at {} differs at byte {i} — {field} (ours \
                     {:#04x}, kernel {:#04x})",
                    key.objectid, ours[i], data[i]
                ));
            }
        });

        if walked.is_err() {
            continue;
        }
        if let Some(msg) = failure {
            panic!("{name}: {msg}");
        }
        if seen > 0 {
            images_checked += 1;
            total += seen;
        }
    }

    assert!(
        images_checked > 0,
        "not one of {} fixtures had a METADATA_ITEM. Every filesystem's own trees are \
         made of tree blocks, so this is a read failure rather than an empty result.",
        images.len()
    );
    eprintln!("{total} allocation records rebuilt across {images_checked} images");
    assert!(
        total > 100,
        "only {total} records is too few to have exercised anything"
    );
}

/// Recording every block group's allocations arrives at the `used` count
/// the kernel wrote.
///
/// The encoder above proves an item is shaped right. This proves the
/// accounting that goes with it lands on the same number the kernel did
/// — starting from an empty group and adding each block back.
#[test]
fn replaying_the_allocations_reaches_the_used_count_the_kernel_recorded() {
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
        let Ok(groups) = fs.block_groups() else {
            continue;
        };

        // Every tree block, by which group holds it.
        let mut blocks: Vec<u64> = Vec::new();
        if fs
            .for_each_extent_item(&mut |key: &DiskKey, _: &[u8]| {
                if key.key_type == key_type::METADATA_ITEM {
                    blocks.push(key.objectid);
                }
            })
            .is_err()
        {
            continue;
        }

        for group in groups
            .iter()
            .filter(|g| g.holds_metadata() && !g.holds_data())
        {
            // Start from empty and add each block this group holds.
            let mut running = BlockGroup { used: 0, ..*group };
            for _ in blocks
                .iter()
                .filter(|&&b| b >= group.start && b < group.end())
            {
                running.used = used_after_allocating(&running, nodesize).unwrap_or_else(|e| {
                    panic!("{name}: replaying the group at {}: {e}", group.start)
                });
            }

            assert_eq!(
                running.used, group.used,
                "{name}: replaying every tree block in the group at {} reaches {} bytes \
                 used, but the kernel recorded {}",
                group.start, running.used, group.used
            );
            checked += 1;
        }
    }

    assert!(
        checked > 0,
        "no metadata-only block group was found to replay"
    );
    eprintln!("{checked} block groups replay to exactly the usage the kernel recorded");
}

/// A filesystem without `SKINNY_METADATA` is refused, not approximated.
#[test]
fn recording_is_refused_without_the_feature_that_defines_the_shape() {
    let Some(fs) = images().iter().find_map(|p| open(p)) else {
        eprintln!("no fixtures — skipping");
        return;
    };

    let mut sb = fs.superblock().clone();
    sb.incompat_flags &= !fs_btrfs::extent_write::INCOMPAT_SKINNY_METADATA;

    let err = record_tree_block(
        &sb,
        TreeBlockAllocation {
            bytenr: 4096,
            level: 0,
            generation: 1,
            owner: 5,
        },
    )
    .expect_err("without the feature the item is a different size");
    assert!(
        err.to_string().contains("SKINNY_METADATA"),
        "the refusal should name the feature it needs: {err}"
    );
}
