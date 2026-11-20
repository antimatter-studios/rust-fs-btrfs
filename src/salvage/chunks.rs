//! The chunk map, built from whatever of the system chunk array and the
//! chunk tree survives.
//!
//! A mount builds the map from the superblock's `sys_chunk_array` and then
//! the chunk tree, and fails if either is damaged. Salvage keeps every
//! chunk it can decode, from both, and names the rest: the system chunks
//! alone reach the chunk tree, and a chunk tree with one bad leaf still
//! maps every chunk its other leaves describe.

use super::tree::{walk_tree, Layout, RootPointer, WalkReport};
use crate::chunk::{chunk_from_item, key_type, objectid, Chunk, ChunkMap, DiskKey, DISK_KEY_SIZE};
use crate::superblock::Superblock;
use fs_core::BlockRead;
use std::ops::ControlFlow;

/// Where a chunk item came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkSource {
    /// The superblock's `sys_chunk_array`.
    SystemArray,
    /// A chunk tree leaf, at this logical address.
    ChunkTree { leaf: u64 },
}

/// A chunk item that was not added to the map, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedChunk {
    pub source: ChunkSource,
    /// The chunk's logical start, from its key.
    pub logical: u64,
    pub reason: String,
}

/// What [`build_chunk_map`] found.
#[derive(Debug, Clone)]
pub struct ChunkScan {
    /// Every chunk that decoded and did not conflict with one already in.
    pub map: ChunkMap,
    /// Chunks taken from the system chunk array.
    pub from_system_array: usize,
    /// Chunks the chunk tree added beyond those.
    pub from_chunk_tree: usize,
    /// Of those, the ones read from a leaf that failed a check. They are
    /// used, because a damaged leaf is still the best evidence of where its
    /// chunks lie, and counted, because nothing verified them.
    pub from_unverified_leaves: usize,
    /// Chunk items that did not decode or overlapped another.
    pub rejected: Vec<RejectedChunk>,
    /// Bytes at the end of the system chunk array that did not decode as
    /// an entry, with the reason. `None` when the whole array decoded.
    pub system_array_tail: Option<String>,
    /// The chunk tree walk, with its damaged blocks.
    pub chunk_tree: WalkReport,
}

/// Add `chunk` unless it overlaps one already in. A chunk identical to one
/// already in is the normal case for system chunks, which both the array
/// and the tree describe, and counts as neither added nor rejected.
fn add(map: &mut ChunkMap, chunk: Chunk) -> Result<bool, String> {
    if let Some(existing) = map.chunk_for(chunk.logical) {
        if *existing == chunk {
            return Ok(false);
        }
    }
    map.insert(chunk).map(|()| true).map_err(|e| e.to_string())
}

/// The system chunk array's entries, as far as they decode.
fn system_array(sb: &Superblock, scan: &mut ChunkScan) {
    let array = &sb.sys_chunk_array;
    let mut at = 0usize;
    while at < array.len() {
        let Ok(key) = DiskKey::parse(&array[at..]) else {
            scan.system_array_tail = Some(format!(
                "{} bytes left at {at}, short of a key",
                array.len() - at
            ));
            return;
        };
        if key.key_type != key_type::CHUNK_ITEM || key.objectid != objectid::FIRST_CHUNK_TREE {
            scan.system_array_tail = Some(format!(
                "entry at {at} has key ({}, {}, {}), not a chunk item",
                key.objectid, key.key_type, key.offset
            ));
            return;
        }
        at += DISK_KEY_SIZE;
        let chunk = match chunk_from_item(&key, &array[at..], sb.sectorsize) {
            Ok(Some(chunk)) => chunk,
            Ok(None) => unreachable!("the key type was checked above"),
            Err(error) => {
                scan.system_array_tail = Some(format!("entry at {at}: {error}"));
                return;
            }
        };
        at += chunk.encoded_len();
        let logical = chunk.logical;
        match add(&mut scan.map, chunk) {
            Ok(true) => scan.from_system_array += 1,
            Ok(false) => {}
            Err(reason) => scan.rejected.push(RejectedChunk {
                source: ChunkSource::SystemArray,
                logical,
                reason,
            }),
        }
    }
}

/// Build the chunk map a superblock copy leads to.
///
/// The system chunk array first, then every chunk item the chunk tree
/// still yields, walked from the copy's `chunk_root` past any damaged
/// block. Never fails: what could not be used is in the result.
pub fn build_chunk_map(source: &dyn BlockRead, sb: &Superblock) -> ChunkScan {
    let mut scan = ChunkScan {
        map: ChunkMap::new(),
        from_system_array: 0,
        from_chunk_tree: 0,
        from_unverified_leaves: 0,
        rejected: Vec::new(),
        system_array_tail: None,
        chunk_tree: WalkReport::default(),
    };
    system_array(sb, &mut scan);

    let mut found = Vec::new();
    let layout = Layout::new(sb, scan.map.clone());
    scan.chunk_tree = walk_tree(
        source,
        &layout,
        RootPointer {
            bytenr: sb.chunk_root,
            generation: Some(sb.chunk_root_generation),
            level: Some(sb.chunk_root_level),
        },
        &mut |leaf| {
            for item in leaf.items() {
                if item.key.key_type != key_type::CHUNK_ITEM {
                    continue;
                }
                let data = leaf.item_data(item).unwrap_or_default();
                found.push((
                    leaf.logical,
                    leaf.is_verified(),
                    item.key,
                    chunk_from_item(&item.key, data, sb.sectorsize),
                ));
            }
            ControlFlow::Continue(())
        },
    );
    for (leaf, verified, key, decoded) in found {
        let source = ChunkSource::ChunkTree { leaf };
        match decoded {
            Ok(Some(chunk)) => match add(&mut scan.map, chunk) {
                Ok(true) => {
                    scan.from_chunk_tree += 1;
                    if !verified {
                        scan.from_unverified_leaves += 1;
                    }
                }
                Ok(false) => {}
                Err(reason) => scan.rejected.push(RejectedChunk {
                    source,
                    logical: key.offset,
                    reason,
                }),
            },
            Ok(None) => {}
            Err(error) => scan.rejected.push(RejectedChunk {
                source,
                logical: key.offset,
                reason: error.to_string(),
            }),
        }
    }
    scan
}
