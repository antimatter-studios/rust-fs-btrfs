//! Editing the kernel's own leaves and putting them back.
//!
//! Recording an allocation is an insert into the extent tree; releasing
//! one is a delete. Both must keep the item list in the order a search
//! bisects on, and an edit that gets that wrong does not fail — it
//! produces a leaf that finds some items and silently misses others.
//!
//! So the check is a round trip on real leaves: take each item out and
//! put it back, and require the list that comes back to be the one that
//! was there. Doing it item by item across every leaf on every fixture
//! covers the positions an edit written as "find the gap" gets wrong —
//! the first, the last, and between two items sharing an objectid.
//!
//! Fixtures are gitignored. Build them with `chore fixtures`.

use fs_btrfs::btree::{header_offsets as o, HEADER_SIZE};
use fs_btrfs::chunk::DiskKey;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::leaf_edit::{delete, fits, insert, OwnedItem};
use fs_btrfs::superblock::Superblock;
use fs_btrfs::tree_write::{build_leaf, chunk_tree_uuid_of, BlockIdentity};
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

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The items of a leaf, owning their bytes.
fn items_of(block: &[u8]) -> Option<Vec<OwnedItem>> {
    let n = le32(block, o::NRITEMS) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let at = HEADER_SIZE + i * 25;
        if at + 25 > block.len() {
            return None;
        }
        let start = HEADER_SIZE + le32(block, at + 17) as usize;
        let end = start + le32(block, at + 21) as usize;
        if end > block.len() {
            return None;
        }
        out.push(OwnedItem {
            key: DiskKey {
                objectid: le64(block, at),
                key_type: block[at + 8],
                offset: le64(block, at + 9),
            },
            data: block[start..end].to_vec(),
        });
    }
    Some(out)
}

/// Every leaf of a filesystem, found by scanning.
fn leaves(img: &Path) -> Option<(Superblock, Vec<Vec<u8>>)> {
    let dev = Arc::new(FileDevice::open(img).ok()?);
    let fs = Filesystem::mount(dev).ok()?;
    let sb = fs.superblock().clone();
    let bytes = std::fs::read(img).ok()?;
    let n = sb.nodesize as usize;
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + n <= bytes.len() {
        let b = &bytes[at..at + n];
        at += n;
        if b[o::FSID..o::FSID + 16] != sb.fsid[..] || b[o::LEVEL] != 0 {
            continue;
        }
        if le32(b, o::NRITEMS) == 0 || !sb.csum_type.verify(&b[32..], &b[..32]) {
            continue;
        }
        out.push(b.to_vec());
    }
    Some((sb, out))
}

/// Taking an item out and putting it back gives the leaf back.
#[test]
fn every_item_of_every_leaf_survives_a_round_trip() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures; build them with `chore fixtures`");
        return;
    }

    let mut round_trips = 0usize;
    let mut leaves_seen = 0usize;

    for img in &images {
        let Some((sb, blocks)) = leaves(img) else {
            continue;
        };
        let name = img.file_name().unwrap().to_string_lossy().into_owned();

        // A sample per image: the round trip is per ITEM, so a handful
        // of leaves is already thousands of edits.
        for block in blocks.iter().take(8) {
            let Some(items) = items_of(block) else {
                continue;
            };
            if items.len() < 2 {
                continue;
            }
            leaves_seen += 1;

            for (i, item) in items.iter().enumerate() {
                let without = delete(&items, &item.key).unwrap_or_else(|e| {
                    panic!("{name}: removing item {i}, which is in the leaf: {e}")
                });
                assert_eq!(
                    without.len(),
                    items.len() - 1,
                    "{name}: removing item {i} changed the count by {}",
                    items.len() as i64 - without.len() as i64
                );

                let back = insert(sb.nodesize, &without, item.clone()).unwrap_or_else(|e| {
                    panic!(
                        "{name}: putting item {i} back into the leaf it came out of: {e}. \
                         It fitted a moment ago."
                    )
                });
                assert_eq!(
                    back, items,
                    "{name}: item {i} went back somewhere other than where it was. The \
                     order is what a search bisects on, so this leaf would find some \
                     items and miss others."
                );
                round_trips += 1;
            }
        }
    }

    if leaves_seen == 0 {
        eprintln!("no readable leaf — skipping");
        return;
    }
    assert!(
        round_trips > 100,
        "only {round_trips} round trips, which is too few to have covered the first, \
         last and middle positions"
    );
    eprintln!("{round_trips} items removed and reinserted across {leaves_seen} kernel leaves");
}

/// A round-tripped leaf encodes to the bytes it started as.
///
/// The test above compares item LISTS. This one closes the loop through
/// the encoder: a leaf that survives an edit as a list but not as bytes
/// is still a leaf the kernel would not have written.
#[test]
fn a_round_tripped_leaf_encodes_to_the_same_bytes() {
    let Some(img) = images().into_iter().next() else {
        eprintln!("no fixtures — skipping");
        return;
    };
    let Some((sb, blocks)) = leaves(&img) else {
        return;
    };

    let mut checked = 0usize;
    for block in blocks.iter().take(20) {
        let Some(items) = items_of(block) else {
            continue;
        };
        if items.len() < 2 {
            continue;
        }

        let id = BlockIdentity {
            bytenr: le64(block, o::BYTENR),
            owner: le64(block, o::OWNER),
            generation: le64(block, o::GENERATION),
            level: 0,
            flags: le64(block, o::FLAGS),
            chunk_tree_uuid: chunk_tree_uuid_of(block),
        };

        // Remove the middle item and put it back.
        let mid = &items[items.len() / 2];
        let without = delete(&items, &mid.key).expect("removing");
        let back = insert(sb.nodesize, &without, mid.clone()).expect("reinserting");

        let borrowed: Vec<_> = back.iter().map(|i| i.as_leaf_item()).collect();
        let ours = build_leaf(&sb, id, &borrowed).expect("encoding the round-tripped leaf");

        // Compare the header and item array; the slack belongs to
        // neither side, as tests/tree_write_oracle.rs explains.
        let items_end = HEADER_SIZE + back.len() * 25;
        assert_eq!(
            ours[32..items_end],
            block[32..items_end],
            "the leaf at {} does not re-encode after a round trip",
            id.bytenr
        );
        checked += 1;
    }
    assert!(checked > 0, "no leaf was round-tripped through the encoder");
    eprintln!("{checked} leaves re-encode identically after an edit");
}

/// An item that will not fit is refused rather than silently dropped.
#[test]
fn an_item_that_needs_a_split_is_refused() {
    let Some(img) = images().into_iter().next() else {
        eprintln!("no fixtures — skipping");
        return;
    };
    let Some((sb, blocks)) = leaves(&img) else {
        return;
    };
    let Some(block) = blocks.first() else { return };
    let Some(items) = items_of(block) else { return };

    // An item as big as the whole block cannot fit alongside anything.
    let huge = OwnedItem {
        key: DiskKey {
            objectid: u64::MAX,
            key_type: 255,
            offset: u64::MAX,
        },
        data: vec![0u8; sb.nodesize as usize],
    };
    assert!(!fits(sb.nodesize, &items, &huge));
    let err = insert(sb.nodesize, &items, huge).expect_err("a block-sized item cannot fit");
    assert!(
        err.to_string()
            .contains("Splitting a leaf is not implemented"),
        "the refusal should name what is missing: {err}"
    );
}
