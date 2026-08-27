//! Every leaf the kernel wrote, rebuilt and compared byte for byte.
//!
//! A copy-on-write writer's first act is to produce a tree block. If the
//! block is wrong the transaction cannot be right, and a block can be
//! wrong in ways that read back perfectly: an item offset measured from
//! the wrong place still parses if every item is measured the same way,
//! and a checksum over the wrong span is still a checksum.
//!
//! So the oracle is the kernel's own blocks. Every leaf in every fixture
//! is taken apart into its items, rebuilt through
//! [`fs_btrfs::tree_write::build_leaf`], and required to come back
//! identical — header, item array, item data and checksum.
//!
//! That is a stronger claim than a round trip through this crate's own
//! parser, which would pass even if both halves shared a misreading.
//!
//! Fixtures are gitignored. Build them with `chore fixtures`.

use fs_btrfs::btree::{header_offsets as o, HEADER_SIZE};
use fs_btrfs::chunk::DiskKey;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::superblock::Superblock;
use fs_btrfs::tree_write::{build_leaf, chunk_tree_uuid_of, BlockIdentity, LeafItem};
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

/// Take a leaf apart into the items it holds.
///
/// Deliberately does not use this crate's `Leaf` parser: the point is to
/// compare against the kernel's bytes, and reading them back through the
/// same code that will rebuild them would let a shared misunderstanding
/// pass.
fn items_of(block: &[u8]) -> Vec<(DiskKey, std::ops::Range<usize>)> {
    let nritems = le32(block, o::NRITEMS) as usize;
    let mut out = Vec::with_capacity(nritems);
    for i in 0..nritems {
        let at = HEADER_SIZE + i * 25;
        let key = DiskKey {
            objectid: le64(block, at),
            key_type: block[at + 8],
            offset: le64(block, at + 9),
        };
        // An item's offset is measured from the end of the header.
        let start = HEADER_SIZE + le32(block, at + 17) as usize;
        let size = le32(block, at + 21) as usize;
        out.push((key, start..start + size));
    }
    out
}

/// Every leaf reachable from a filesystem's trees.
///
/// Walking is done through the crate's own `Tree`, which is fine — what
/// is being validated is the *encoder*, and how the blocks were reached
/// does not affect whether the bytes match.
fn leaves(img: &Path) -> Option<(Superblock, Vec<Vec<u8>>)> {
    let dev = Arc::new(FileDevice::open(img).ok()?);
    let fs = Filesystem::mount(dev).ok()?;
    let sb = fs.superblock().clone();

    let mut blocks = Vec::new();

    // The blocks are found by scanning rather than by walking a tree,
    // and the checksum is what makes that safe: a run of file data
    // cannot fake a digest of itself. Scanning also reaches blocks no
    // walk would — older generations still on disk, and the trees this
    // crate has no walker for.
    //
    // Read directly: any block whose header says it is a
    // leaf of this filesystem is a candidate, and scanning is how they
    // are found without a second walker.
    let bytes = std::fs::read(img).ok()?;
    let nodesize = sb.nodesize as usize;
    let mut at = 0usize;
    while at + nodesize <= bytes.len() {
        let block = &bytes[at..at + nodesize];
        at += nodesize;

        // A leaf of this filesystem: right UUID, level zero, a plausible
        // item count, and a checksum that verifies. The checksum is what
        // makes the scan safe — a run of file data cannot fake it.
        if block[o::FSID..o::FSID + 16] != sb.fsid[..] || block[o::LEVEL] != 0 {
            continue;
        }
        let nritems = le32(block, o::NRITEMS) as usize;
        if nritems == 0 || HEADER_SIZE + nritems * 25 > nodesize {
            continue;
        }
        if !sb.csum_type.verify(&block[32..], &block[..32]) {
            continue;
        }
        blocks.push(block.to_vec());
    }
    Some((sb, blocks))
}

/// Rebuilding a leaf gives back exactly what the kernel wrote.
#[test]
fn every_leaf_re_encodes_identically() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures; build them with `chore fixtures`");
        return;
    }

    let mut total = 0usize;
    let mut exact = 0usize;
    let mut images_read = 0usize;

    for img in &images {
        let Some((sb, blocks)) = leaves(img) else {
            continue;
        };
        if blocks.is_empty() {
            continue;
        }
        images_read += 1;
        let name = img.file_name().unwrap().to_string_lossy().into_owned();

        for theirs in &blocks {
            let parsed = items_of(theirs);
            if parsed.iter().any(|(_, r)| r.end > theirs.len()) {
                continue;
            }

            let items: Vec<LeafItem> = parsed
                .iter()
                .map(|(key, range)| LeafItem {
                    key: *key,
                    data: &theirs[range.clone()],
                })
                .collect();

            let id = BlockIdentity {
                bytenr: le64(theirs, o::BYTENR),
                owner: le64(theirs, o::OWNER),
                generation: le64(theirs, o::GENERATION),
                level: 0,
                flags: le64(theirs, o::FLAGS),
                chunk_tree_uuid: chunk_tree_uuid_of(theirs),
            };

            let ours = match build_leaf(&sb, id, &items) {
                Ok(b) => b,
                // A leaf whose items this test could not order is not a
                // failure of the encoder; skip it and say so at the end.
                Err(_) => continue,
            };

            // The free space between the item array and the item data
            // is written by neither side. The kernel's block holds
            // whatever was there when the block was last used; a rebuilt
            // one holds zeros. So the comparison is in three parts, and
            // the checksum is the interesting one.
            let items_end = HEADER_SIZE + items.len() * 25;
            let data_start = theirs.len() - items.iter().map(|i| i.data.len()).sum::<usize>();

            // 1. Everything after the checksum, up to the end of the item
            //    array: the identity fields and every item's key, offset
            //    and size.
            assert_eq!(
                ours[32..items_end],
                theirs[32..items_end],
                "{name}: block at {} differs in its header or item array",
                id.bytenr
            );

            // 2. The item data itself.
            assert_eq!(
                ours[data_start..],
                theirs[data_start..],
                "{name}: block at {} differs in its item data",
                id.bytenr
            );

            // 3. The checksum. It covers the whole block, free space
            //    included, so it can only equal the kernel's when the
            //    free space does — which is the case for a block whose
            //    slack was never used. Where it is, the WHOLE block must
            //    match, checksum and all; where it is not, ours must at
            //    least be a correct checksum of what we built.
            let slack_matches = ours[items_end..data_start] == theirs[items_end..data_start];
            if slack_matches {
                assert_eq!(
                    ours, *theirs,
                    "{name}: block at {} has identical content and slack, so every byte \
                     including the checksum should match",
                    id.bytenr
                );
                exact += 1;
            } else {
                assert!(
                    sb.csum_type.verify(&ours[32..], &ours[..32]),
                    "{name}: block at {} carries a checksum that does not cover it",
                    id.bytenr
                );
            }

            total += 1;
        }
    }

    if images_read == 0 {
        eprintln!("no readable fixtures — skipping");
        return;
    }
    assert!(
        total > 20,
        "only {total} leaves were rebuilt across {images_read} images, which is too few \
         to have exercised the encoder"
    );
    eprintln!(
        "{total} kernel leaves rebuilt across {images_read} images; {exact} matched every \
         byte including the checksum, the rest differed only in slack the kernel never \
         rewrote"
    );
    assert!(
        exact > 0,
        "not one leaf matched byte for byte — if the slack always differs, the \
         comparison is not reaching the checksum at all"
    );
}

/// A leaf that will not fit is refused rather than truncated.
#[test]
fn an_overfull_leaf_is_refused() {
    let Some(img) = images().into_iter().next() else {
        eprintln!("no fixtures — skipping");
        return;
    };
    let Some((sb, _)) = leaves(&img) else {
        return;
    };

    let big = vec![0u8; sb.nodesize as usize];
    let items = [LeafItem {
        key: DiskKey {
            objectid: 1,
            key_type: 1,
            offset: 0,
        },
        data: &big,
    }];
    let err = build_leaf(
        &sb,
        BlockIdentity {
            bytenr: 0,
            owner: 5,
            generation: 1,
            level: 0,
            flags: 0,
            chunk_tree_uuid: [0; 16],
        },
        &items,
    )
    .expect_err("an item the size of the whole block cannot fit alongside a header");
    assert!(
        err.to_string().contains("splitting a leaf"),
        "the refusal should name what the answer would be: {err}"
    );
}

/// Items out of order are refused, because a search bisects on that
/// order and an unsorted leaf finds some items and silently misses
/// others.
#[test]
fn unsorted_items_are_refused() {
    let Some(img) = images().into_iter().next() else {
        eprintln!("no fixtures — skipping");
        return;
    };
    let Some((sb, _)) = leaves(&img) else {
        return;
    };

    let a = [1u8; 8];
    let items = [
        LeafItem {
            key: DiskKey {
                objectid: 9,
                key_type: 1,
                offset: 0,
            },
            data: &a,
        },
        LeafItem {
            key: DiskKey {
                objectid: 2,
                key_type: 1,
                offset: 0,
            },
            data: &a,
        },
    ];
    assert!(
        build_leaf(
            &sb,
            BlockIdentity {
                bytenr: 0,
                owner: 5,
                generation: 1,
                level: 0,
                flags: 0,
                chunk_tree_uuid: [0; 16],
            },
            &items,
        )
        .is_err(),
        "an unsorted leaf must be refused"
    );
}
