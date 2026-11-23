//! Data checksums from the checksum tree, looked up past its damage.
//!
//! The checksum tree records one digest per data sector, filed under the
//! sector's logical address. A salvaged byte range is checked against
//! them sector by sector: a sector whose digest matches is the data as
//! written, one whose digest does not is evidence of damage, and one with
//! no digest (a `nodatasum` file, or a checksum leaf that is itself lost)
//! is unchecked.

use super::tree::{read_block, Expect, Layout, RootPointer, SalvagedBlock, SalvagedBody};
use crate::btree::compare_keys;
use crate::chunk::DiskKey;
use crate::csum::{EXTENT_CSUM_KEY, EXTENT_CSUM_OBJECTID};
use crate::superblock::CSUM_SIZE;
use fs_core::BlockRead;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

/// Tree blocks a [`CsumTree`] keeps between lookups.
const CACHE_BLOCKS: usize = 512;

/// What the checksum tree records for one data sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectorCsum {
    /// A digest, and whether the leaf it came from verified.
    Recorded {
        digest: [u8; CSUM_SIZE],
        leaf_verified: bool,
    },
    /// No item covers the sector: the data has no checksum.
    Missing,
    /// The path to the sector's leaf could not be read, so whether it has
    /// a checksum is unknown.
    Unreadable,
}

/// The checksum tree of one root, with a small cache of the blocks a
/// lookup reads.
pub struct CsumTree {
    root: RootPointer,
    cache: RefCell<HashMap<u64, Rc<SalvagedBlock>>>,
}

impl CsumTree {
    /// The tree whose root the root tree's `ROOT_ITEM` 7 names.
    pub fn new(root: RootPointer) -> Self {
        Self {
            root,
            cache: RefCell::new(HashMap::new()),
        }
    }

    fn block(
        &self,
        source: &dyn BlockRead,
        layout: &Layout,
        logical: u64,
        expect: Expect,
    ) -> Rc<SalvagedBlock> {
        if let Some(block) = self.cache.borrow().get(&logical) {
            return Rc::clone(block);
        }
        let block = Rc::new(read_block(source, layout, logical, expect));
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= CACHE_BLOCKS {
            cache.clear();
        }
        cache.insert(logical, Rc::clone(&block));
        block
    }

    /// What the tree records for the sector at `logical`.
    pub fn lookup(&self, source: &dyn BlockRead, layout: &Layout, logical: u64) -> SectorCsum {
        let target = DiskKey {
            objectid: EXTENT_CSUM_OBJECTID,
            key_type: EXTENT_CSUM_KEY,
            offset: logical,
        };
        let mut block = self.block(
            source,
            layout,
            self.root.bytenr,
            Expect {
                generation: self.root.generation,
                level: self.root.level,
            },
        );
        for _ in 0..crate::superblock::MAX_LEVEL {
            match &block.body {
                SalvagedBody::None => return SectorCsum::Unreadable,
                SalvagedBody::Node(pointers) => {
                    let slot = pointers
                        .iter()
                        .rposition(|p| compare_keys(&p.key, &target) != Ordering::Greater)
                        .unwrap_or(0);
                    let Some(pointer) = pointers.get(slot) else {
                        return SectorCsum::Unreadable;
                    };
                    let level = block.header.as_ref().map_or(1, |h| h.level);
                    block = self.block(
                        source,
                        layout,
                        pointer.blockptr,
                        Expect {
                            generation: Some(pointer.generation),
                            level: level.checked_sub(1),
                        },
                    );
                }
                SalvagedBody::Leaf(items) => {
                    let size = layout.geometry.csum_type.digest_len();
                    let sector = u64::from(layout.sectorsize);
                    let item = items.iter().rev().find(|item| {
                        item.key.objectid == EXTENT_CSUM_OBJECTID
                            && item.key.key_type == EXTENT_CSUM_KEY
                            && item.key.offset <= logical
                    });
                    let Some(item) = item else {
                        return SectorCsum::Missing;
                    };
                    let index = (logical - item.key.offset) / sector;
                    let start = index as usize * size;
                    let data = block.item_data(item).unwrap_or_default();
                    return match data.get(start..start + size) {
                        Some(digest) => {
                            let mut stored = [0u8; CSUM_SIZE];
                            stored[..size].copy_from_slice(digest);
                            SectorCsum::Recorded {
                                digest: stored,
                                leaf_verified: block.is_verified(),
                            }
                        }
                        None => SectorCsum::Missing,
                    };
                }
            }
        }
        SectorCsum::Unreadable
    }
}

/// How a run of sectors checked against the checksum tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SectorTally {
    /// Sectors whose digest matched.
    pub verified: u64,
    /// Sectors with no digest, or whose digest could not be read.
    pub unchecked: u64,
    /// Sectors whose digest did not match on any copy.
    pub failed: u64,
}

impl SectorTally {
    pub fn add(&mut self, other: SectorTally) {
        self.verified += other.verified;
        self.unchecked += other.unchecked;
        self.failed += other.failed;
    }
}

/// Check `bytes`, read from `logical`, against the checksum tree, sector
/// by sector. A sector that fails is read again from the other copies and
/// replaced by the first that matches; one that matches on no copy is kept
/// as read, and its `(logical, length)` is returned.
///
/// Only whole sectors are checked; a partial sector at either end counts
/// as unchecked.
pub fn verify_data(
    source: &dyn BlockRead,
    layout: &Layout,
    csums: &CsumTree,
    logical: u64,
    bytes: &mut [u8],
) -> (SectorTally, Vec<(u64, u64)>) {
    let sector = u64::from(layout.sectorsize);
    let csum_type = layout.geometry.csum_type;
    let mut tally = SectorTally::default();
    let mut failed: Vec<(u64, u64)> = Vec::new();
    let mut at = 0u64;
    let end = bytes.len() as u64;
    while at < end {
        let address = logical + at;
        let lead = (sector - address % sector) % sector;
        if lead != 0 || end - at < sector {
            let take = if lead != 0 {
                lead.min(end - at)
            } else {
                end - at
            };
            tally.unchecked += 1;
            at += take;
            continue;
        }
        let range = at as usize..(at + sector) as usize;
        match csums.lookup(source, layout, address) {
            SectorCsum::Recorded { digest, .. } => {
                if csum_type.verify(&bytes[range.clone()], &digest) {
                    tally.verified += 1;
                } else {
                    let mut repaired = false;
                    let mut copy = vec![0u8; sector as usize];
                    for mirror in 1..layout.copies(address) {
                        if layout
                            .read_logical(source, address, mirror, &mut copy)
                            .is_ok()
                            && csum_type.verify(&copy, &digest)
                        {
                            bytes[range.clone()].copy_from_slice(&copy);
                            repaired = true;
                            break;
                        }
                    }
                    if repaired {
                        tally.verified += 1;
                    } else {
                        tally.failed += 1;
                        match failed.last_mut() {
                            Some(last) if last.0 + last.1 == address => last.1 += sector,
                            _ => failed.push((address, sector)),
                        }
                    }
                }
            }
            SectorCsum::Missing | SectorCsum::Unreadable => tally.unchecked += 1,
        }
        at += sector;
    }
    (tally, failed)
}
