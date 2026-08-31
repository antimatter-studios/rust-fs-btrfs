//! Every internal node the kernel wrote, rebuilt and compared.
//!
//! Copy-on-write means a change to one leaf rewrites the whole spine
//! above it: a new leaf, a new node pointing at it, a new node above
//! that, up to the root. So the nodes are half of what a transaction
//! produces, and a node can be wrong in a way that reads back perfectly
//! — every pointer parses, the descent terminates, and a search returns
//! "not found" for a key that is there.
//!
//! The same oracle as the leaves: take the kernel's own nodes apart,
//! rebuild them through [`fs_btrfs::tree_write::build_node`], and
//! require the bytes back.
//!
//! Fixtures are gitignored. Build them with `chore fixtures`.

use fs_btrfs::btree::{header_offsets as o, KeyPtr, HEADER_SIZE, KEY_PTR_SIZE};
use fs_btrfs::chunk::DiskKey;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::superblock::Superblock;
use fs_btrfs::tree_write::{build_node, chunk_tree_uuid_of, key_ptr_capacity, BlockIdentity};
use fs_core::FileDevice;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod common;
use common::{le32, le64};

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

/// Take a node apart into its key pointers.
///
/// Read here rather than through the crate's `KeyPtr::parse` for the
/// same reason the leaf oracle re-implements its own reader: a shared
/// misunderstanding of the packed layout would cancel out and pass.
fn ptrs_of(block: &[u8]) -> Vec<KeyPtr> {
    let nritems = le32(block, o::NRITEMS) as usize;
    (0..nritems)
        .map(|i| {
            let at = HEADER_SIZE + i * KEY_PTR_SIZE;
            KeyPtr {
                key: DiskKey {
                    objectid: le64(block, at),
                    key_type: block[at + 8],
                    offset: le64(block, at + 9),
                },
                // Packed: both u64s start on odd offsets, 17 and 25.
                blockptr: le64(block, at + 17),
                generation: le64(block, at + 25),
            }
        })
        .collect()
}

/// Every internal node on a filesystem, found by scanning.
///
/// Scanning rather than walking, so it reaches nodes no walk from the
/// current root would — older generations still on disk, and the trees
/// this crate has no walker for. The checksum is what makes that safe:
/// a run of file data cannot fake a digest of itself.
fn nodes(img: &Path) -> Option<(Superblock, Vec<Vec<u8>>)> {
    let dev = Arc::new(FileDevice::open(img).ok()?);
    let fs = Filesystem::mount(dev).ok()?;
    let sb = fs.superblock().clone();

    let bytes = std::fs::read(img).ok()?;
    let nodesize = sb.nodesize as usize;
    let mut blocks = Vec::new();
    let mut at = 0usize;
    while at + nodesize <= bytes.len() {
        let block = &bytes[at..at + nodesize];
        at += nodesize;

        // A node of this filesystem: right UUID, level above zero, an
        // item count that fits, and a checksum that verifies.
        if block[o::FSID..o::FSID + 16] != sb.fsid[..] || block[o::LEVEL] == 0 {
            continue;
        }
        let nritems = le32(block, o::NRITEMS) as usize;
        if nritems == 0 || HEADER_SIZE + nritems * KEY_PTR_SIZE > nodesize {
            continue;
        }
        if !sb.csum_type.verify(&block[32..], &block[..32]) {
            continue;
        }
        blocks.push(block.to_vec());
    }
    Some((sb, blocks))
}

/// Rebuilding a node gives back exactly what the kernel wrote.
#[test]
fn every_node_re_encodes_identically() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures; build them with `chore fixtures`");
        return;
    }

    let mut total = 0usize;
    let mut exact = 0usize;
    let mut images_with_nodes = 0usize;
    let mut deepest = 0u8;

    for img in &images {
        let Some((sb, blocks)) = nodes(img) else {
            continue;
        };
        if blocks.is_empty() {
            continue;
        }
        images_with_nodes += 1;
        let name = img.file_name().unwrap().to_string_lossy().into_owned();

        for theirs in &blocks {
            let ptrs = ptrs_of(theirs);
            let level = theirs[o::LEVEL];
            deepest = deepest.max(level);

            let id = BlockIdentity {
                bytenr: le64(theirs, o::BYTENR),
                owner: le64(theirs, o::OWNER),
                generation: le64(theirs, o::GENERATION),
                level,
                flags: le64(theirs, o::FLAGS),
                chunk_tree_uuid: chunk_tree_uuid_of(theirs),
            };

            let ours = match build_node(&sb, id, &ptrs) {
                Ok(b) => b,
                Err(e) => panic!(
                    "{name}: node at {} was refused: {e}. The kernel wrote it, so \
                     refusing it names a rule this encoder has that the format does not.",
                    id.bytenr
                ),
            };

            // Header and pointer array must match exactly. The slack
            // after them holds whatever the block last held, so the
            // checksum can only match when the slack does — the same
            // three-part comparison the leaf oracle makes, for the same
            // reason.
            let ptrs_end = HEADER_SIZE + ptrs.len() * KEY_PTR_SIZE;
            // Named byte by byte rather than by comparing the slices
            // directly: a failed `assert_eq!` on two 16 KiB blocks
            // prints both of them, and the one number that identifies
            // the bug is the offset.
            if let Some(i) = (32..ptrs_end).find(|&i| ours[i] != theirs[i]) {
                let (what, within) = if i < HEADER_SIZE {
                    ("the header", i)
                } else {
                    ("key pointer", (i - HEADER_SIZE) % KEY_PTR_SIZE)
                };
                panic!(
                    "{name}: node at {} differs at byte {i:#x} — {what}, offset {within} \
                     into it (ours {:#04x}, kernel {:#04x}). A key pointer is a 17-byte \
                     key, then blockptr at 17 and generation at 25.",
                    id.bytenr, ours[i], theirs[i]
                );
            }

            if ours[ptrs_end..] == theirs[ptrs_end..] {
                assert_eq!(
                    ours, *theirs,
                    "{name}: node at {} has identical content and slack, so every byte \
                     including the checksum should match",
                    id.bytenr
                );
                exact += 1;
            } else {
                assert!(
                    sb.csum_type.verify(&ours[32..], &ours[..32]),
                    "{name}: node at {} carries a checksum that does not cover it",
                    id.bytenr
                );
            }
            total += 1;
        }
    }

    // Fixtures exist but none had a node: that is a regression in the
    // fixture matrix, not a reason to pass. A skip here reads exactly
    // like success, which is how the leaf oracle went a release running
    // against nothing.
    assert!(
        images_with_nodes > 0,
        "{} fixtures were read and not one had a tree above level 0, so this test \
         exercised nothing. The deep geometries in scripts/fixture-geometries.sh are \
         what produce nodes.",
        images.len()
    );
    eprintln!(
        "{total} kernel nodes rebuilt across {images_with_nodes} images, deepest level \
         {deepest}; {exact} matched every byte including the checksum"
    );
    assert!(
        exact > 0,
        "not one node matched byte for byte — if the slack always differs, the comparison \
         never reaches the checksum"
    );
}

/// The keys in a kernel node really are the smallest key of the child.
///
/// This is the rule a node encoder can break without breaking anything
/// that parses: descent takes the last child whose key is <= the one
/// sought, so a key that is too large skips a subtree silently. Checked
/// against the kernel's own trees rather than asserted in prose.
#[test]
fn each_pointer_key_is_the_first_key_of_the_child_it_names() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }

    let mut checked = 0usize;
    for img in &images {
        let Some((sb, blocks)) = nodes(img) else {
            continue;
        };
        if blocks.is_empty() {
            continue;
        }
        let Ok(bytes) = std::fs::read(img) else {
            continue;
        };

        // Address every block by its own recorded bytenr, so a child can
        // be found without resolving logical addresses through the chunk
        // tree — which is what makes this check independent of the
        // reader being validated.
        let nodesize = sb.nodesize as usize;
        let mut by_bytenr = std::collections::HashMap::new();
        let mut at = 0usize;
        while at + nodesize <= bytes.len() {
            let b = &bytes[at..at + nodesize];
            at += nodesize;
            if b[o::FSID..o::FSID + 16] == sb.fsid[..]
                && sb.csum_type.verify(&b[32..], &b[..32])
                && le32(b, o::NRITEMS) > 0
            {
                by_bytenr.insert(le64(b, o::BYTENR), b.to_vec());
            }
        }

        for node in &blocks {
            for ptr in ptrs_of(node) {
                let Some(child) = by_bytenr.get(&ptr.blockptr) else {
                    continue;
                };
                // Only compare against a child of the generation this
                // pointer names — an older block at that address is a
                // different tree's, not this pointer's target.
                if le64(child, o::GENERATION) != ptr.generation {
                    continue;
                }
                // The child's first key, whether it is a leaf or a node:
                // both start their array at the same offset and both
                // open it with a 17-byte key.
                let first = DiskKey {
                    objectid: le64(child, HEADER_SIZE),
                    key_type: child[HEADER_SIZE + 8],
                    offset: le64(child, HEADER_SIZE + 9),
                };
                assert_eq!(
                    (ptr.key.objectid, ptr.key.key_type, ptr.key.offset),
                    (first.objectid, first.key_type, first.offset),
                    "a pointer to {} carries {:?} but the child's first key is {:?}",
                    ptr.blockptr,
                    ptr.key,
                    first
                );
                checked += 1;
            }
        }
    }

    assert!(
        checked > 0,
        "not one parent/child pair was resolvable across {} fixtures, so the descent \
         invariant was never checked",
        images.len()
    );
    eprintln!("{checked} pointer keys are the first key of the child they name");
}

/// A node at level 0 is refused: that is a leaf, and a block claiming
/// both is one a reader walks off the bottom of.
#[test]
fn a_node_cannot_claim_to_be_a_leaf() {
    let Some(img) = images().into_iter().next() else {
        eprintln!("no fixtures — skipping");
        return;
    };
    let Some((sb, _)) = nodes(&img).or_else(|| {
        // Any fixture will do — this needs a superblock, not a node.
        images().iter().find_map(|p| nodes(p))
    }) else {
        eprintln!("no readable fixture — skipping");
        return;
    };

    let ptr = KeyPtr {
        key: DiskKey {
            objectid: 1,
            key_type: 1,
            offset: 0,
        },
        blockptr: 4096,
        generation: 1,
    };
    let err = build_node(
        &sb,
        BlockIdentity {
            bytenr: 0,
            owner: 1,
            generation: 1,
            level: 0,
            flags: 0,
            chunk_tree_uuid: [0; 16],
        },
        &[ptr],
    )
    .expect_err("level 0 with key pointers is a contradiction");
    assert!(
        err.to_string().contains("level 0"),
        "the refusal should name the contradiction: {err}"
    );
}

/// More pointers than fit are refused rather than truncated.
#[test]
fn an_overfull_node_is_refused() {
    let Some((sb, _)) = images().iter().find_map(|p| nodes(p)) else {
        eprintln!("no fixtures — skipping");
        return;
    };

    let capacity = key_ptr_capacity(&sb);
    let ptrs: Vec<KeyPtr> = (0..capacity as u64 + 1)
        .map(|i| KeyPtr {
            key: DiskKey {
                objectid: i,
                key_type: 1,
                offset: 0,
            },
            blockptr: 4096 * (i + 1),
            generation: 1,
        })
        .collect();

    assert!(
        build_node(
            &sb,
            BlockIdentity {
                bytenr: 0,
                owner: 1,
                generation: 1,
                level: 1,
                flags: 0,
                chunk_tree_uuid: [0; 16],
            },
            &ptrs,
        )
        .is_err(),
        "one more than capacity ({capacity}) must be refused"
    );

    // And exactly capacity must be accepted, or the bound is off by one
    // in the direction that wastes a slot on every node.
    assert!(
        build_node(
            &sb,
            BlockIdentity {
                bytenr: 0,
                owner: 1,
                generation: 1,
                level: 1,
                flags: 0,
                chunk_tree_uuid: [0; 16],
            },
            &ptrs[..capacity],
        )
        .is_ok(),
        "exactly capacity ({capacity}) must fit"
    );
}
