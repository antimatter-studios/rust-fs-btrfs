//! Tree blocks read whatever their state, and trees walked past their
//! damage.
//!
//! [`crate::btree::TreeBlock::parse`] is all or nothing: a block whose
//! checksum fails yields an error and no items. Here a block yields its
//! header and whatever of its body still decodes, with a [`BlockFault`]
//! for every check it failed, and [`walk_tree`] records a damaged block
//! and carries on with the rest of the tree rather than stopping.

use crate::btree::{
    compare_keys, Header, Item, KeyPtr, TreeGeometry, HEADER_SIZE, ITEM_SIZE, KEY_PTR_SIZE,
    LEAF_DATA_OFFSET,
};
use crate::chunk::ChunkMap;
use crate::superblock::{Superblock, CSUM_SIZE, MAX_LEVEL};
use fs_core::BlockRead;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::ops::ControlFlow;

/// What salvage knows about a volume's layout: plain data, no device.
///
/// Built from one superblock copy and a chunk map. The source it reads is
/// passed to every call, so a `Layout` can never write and never outlives
/// the read it was made for.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Node size, checksum type and the fsid tree blocks carry.
    pub geometry: TreeGeometry,
    /// The data sector size.
    pub sectorsize: u32,
    /// The device id of the source, from the superblock's device item.
    /// A stripe on another device cannot be read from this source.
    pub devid: u64,
    /// Logical to physical translation.
    pub chunks: ChunkMap,
}

impl Layout {
    /// The layout a superblock describes, over `chunks`.
    pub fn new(superblock: &Superblock, chunks: ChunkMap) -> Self {
        Self {
            geometry: TreeGeometry::from_superblock(superblock),
            sectorsize: superblock.sectorsize,
            devid: superblock.dev_item.devid,
            chunks,
        }
    }

    /// How many copies of `logical` the volume keeps; 0 when no chunk
    /// covers it.
    pub fn copies(&self, logical: u64) -> usize {
        self.chunks.read_attempts_at(logical).unwrap_or(0)
    }

    /// Read `buf.len()` bytes at `logical` from copy `mirror`, following
    /// the mapping across stripe boundaries.
    pub fn read_logical(
        &self,
        source: &dyn BlockRead,
        logical: u64,
        mirror: usize,
        buf: &mut [u8],
    ) -> Result<(), BlockFault> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = logical + done as u64;
            let mapping = self
                .chunks
                .map_mirror(at, mirror)
                .map_err(|_| BlockFault::Unmapped)?;
            if mapping.devid != self.devid {
                return Err(BlockFault::OtherDevice {
                    devid: mapping.devid,
                });
            }
            let take = (buf.len() - done).min(mapping.len.max(1) as usize);
            source
                .read_at(mapping.physical, &mut buf[done..done + take])
                .map_err(|error| BlockFault::Unreadable(error.to_string()))?;
            done += take;
        }
        Ok(())
    }
}

impl Layout {
    /// The physical `(offset, length)` runs on the source that copy
    /// `mirror` of `[logical, logical + length)` occupies, in order.
    pub fn physical_runs(
        &self,
        logical: u64,
        length: u64,
        mirror: usize,
    ) -> Result<Vec<(u64, u64)>, BlockFault> {
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let mut done = 0u64;
        while done < length {
            let mapping = self
                .chunks
                .map_mirror(logical + done, mirror)
                .map_err(|_| BlockFault::Unmapped)?;
            if mapping.devid != self.devid {
                return Err(BlockFault::OtherDevice {
                    devid: mapping.devid,
                });
            }
            let take = (length - done).min(mapping.len.max(1));
            match runs.last_mut() {
                Some(last) if last.0 + last.1 == mapping.physical => last.1 += take,
                _ => runs.push((mapping.physical, take)),
            }
            done += take;
        }
        Ok(runs)
    }
}

/// One check a tree block failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockFault {
    /// No chunk maps the address.
    Unmapped,
    /// The address maps to a device other than the source.
    OtherDevice { devid: u64 },
    /// The device failed the read.
    Unreadable(String),
    /// The stored checksum disagrees with the block.
    Checksum,
    /// The header names another address as its own.
    Bytenr { found: u64 },
    /// The header carries another filesystem's fsid.
    Fsid,
    /// The parent recorded another generation for this block: a stale or
    /// reused block reached through an older pointer.
    Generation { expected: u64, found: u64 },
    /// The block sits at a level its parent does not allow.
    Level { expected: u8, found: u8 },
    /// The body does not hold together: an item outside the block, keys
    /// out of order, a null child pointer.
    Malformed(String),
}

/// What decoded of a block's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SalvagedBody {
    /// Nothing: the block could not be read or its header is unusable.
    None,
    /// An internal node's child pointers that decoded.
    Node(Vec<KeyPtr>),
    /// A leaf's items whose data lies inside the block.
    Leaf(Vec<Item>),
}

/// What one copy of a block said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyAttempt {
    pub mirror: usize,
    pub faults: Vec<BlockFault>,
}

/// A tree block as salvage read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SalvagedBlock {
    /// The address the block was read from.
    pub logical: u64,
    /// The copy whose bytes this is.
    pub mirror: usize,
    /// The header, when the bytes were read.
    pub header: Option<Header>,
    /// Every check this copy failed. Empty means the block verifies as
    /// the mount would verify it.
    pub faults: Vec<BlockFault>,
    pub body: SalvagedBody,
    /// Every copy tried, in order, with its faults. A volume with two
    /// copies of metadata lists both even when the first verifies only
    /// after the second was tried.
    pub attempts: Vec<CopyAttempt>,
    bytes: Vec<u8>,
}

impl SalvagedBlock {
    /// Whether this copy passed every check.
    pub fn is_verified(&self) -> bool {
        self.faults.is_empty()
    }

    /// The block's bytes, empty when it could not be read.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// A leaf item's data. Every item in [`SalvagedBody::Leaf`] lies inside
    /// the block, so this is `None` only for an item from elsewhere.
    pub fn item_data(&self, item: &Item) -> Option<&[u8]> {
        let start = LEAF_DATA_OFFSET.checked_add(item.offset as usize)?;
        let end = start.checked_add(item.size as usize)?;
        self.bytes.get(start..end)
    }

    /// The leaf items, empty for a node or an unreadable block.
    pub fn items(&self) -> &[Item] {
        match &self.body {
            SalvagedBody::Leaf(items) => items,
            _ => &[],
        }
    }
}

/// What a parent says about a block before it is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Expect {
    /// The generation the parent recorded.
    pub generation: Option<u64>,
    /// The level the block must sit at.
    pub level: Option<u8>,
}

/// Decode a block read from `logical` as far as it goes, recording each
/// check it fails.
pub fn decode_block(
    bytes: &[u8],
    logical: u64,
    geometry: &TreeGeometry,
    expect: Expect,
) -> (Option<Header>, Vec<BlockFault>, SalvagedBody) {
    let nodesize = geometry.nodesize as usize;
    let mut faults = Vec::new();
    if bytes.len() != nodesize || nodesize < HEADER_SIZE {
        faults.push(BlockFault::Malformed(format!(
            "{} bytes for a {nodesize}-byte node",
            bytes.len()
        )));
        return (None, faults, SalvagedBody::None);
    }
    let header = Header::parse(bytes).expect("the block holds a whole header");
    if !geometry
        .csum_type
        .verify(&bytes[CSUM_SIZE..nodesize], &header.csum)
    {
        faults.push(BlockFault::Checksum);
    }
    if header.bytenr != logical {
        faults.push(BlockFault::Bytenr {
            found: header.bytenr,
        });
    }
    if header.fsid != geometry.fsid {
        faults.push(BlockFault::Fsid);
    }
    if let Some(expected) = expect.generation {
        if header.generation != expected {
            faults.push(BlockFault::Generation {
                expected,
                found: header.generation,
            });
        }
    }
    if let Some(expected) = expect.level {
        if header.level != expected {
            faults.push(BlockFault::Level {
                expected,
                found: header.level,
            });
        }
    }
    if header.level >= MAX_LEVEL {
        faults.push(BlockFault::Malformed(format!(
            "level {} is at or above the maximum tree height {MAX_LEVEL}",
            header.level
        )));
        return (Some(header), faults, SalvagedBody::None);
    }
    let body = if header.is_leaf() {
        SalvagedBody::Leaf(decode_items(bytes, &header, &mut faults))
    } else {
        SalvagedBody::Node(decode_key_ptrs(bytes, &header, &mut faults))
    };
    (Some(header), faults, body)
}

/// The items whose data lies inside the block. One out of place is left
/// out and named; keys out of order are named and kept.
fn decode_items(bytes: &[u8], header: &Header, faults: &mut Vec<BlockFault>) -> Vec<Item> {
    let data_size = bytes.len() - LEAF_DATA_OFFSET;
    let fit = data_size / ITEM_SIZE;
    let count = header.nritems as usize;
    if count > fit {
        faults.push(BlockFault::Malformed(format!(
            "nritems {count} is more than the {fit} items a block holds"
        )));
    }
    let array_end = count.min(fit) * ITEM_SIZE;
    let mut items: Vec<Item> = Vec::with_capacity(count.min(fit));
    let mut out_of_order = false;
    for index in 0..count.min(fit) {
        let at = LEAF_DATA_OFFSET + index * ITEM_SIZE;
        let item = Item::parse(&bytes[at..]).expect("the item lies inside the block");
        if item.data_end() > data_size || (item.offset as usize) < array_end {
            faults.push(BlockFault::Malformed(format!(
                "item {index} data [{}, {}) is outside the data area",
                item.offset,
                item.data_end()
            )));
            continue;
        }
        if items
            .last()
            .is_some_and(|previous| compare_keys(&previous.key, &item.key) != Ordering::Less)
        {
            out_of_order = true;
        }
        items.push(item);
    }
    if out_of_order {
        faults.push(BlockFault::Malformed("item keys do not ascend".into()));
    }
    items
}

/// The child pointers that name a block. A null one is left out and named;
/// keys out of order are named and kept.
fn decode_key_ptrs(bytes: &[u8], header: &Header, faults: &mut Vec<BlockFault>) -> Vec<KeyPtr> {
    let fit = (bytes.len() - HEADER_SIZE) / KEY_PTR_SIZE;
    let count = header.nritems as usize;
    if count == 0 {
        faults.push(BlockFault::Malformed(
            "internal node has no children".into(),
        ));
    }
    if count > fit {
        faults.push(BlockFault::Malformed(format!(
            "nritems {count} is more than the {fit} pointers a block holds"
        )));
    }
    let mut pointers: Vec<KeyPtr> = Vec::with_capacity(count.min(fit));
    let mut out_of_order = false;
    for index in 0..count.min(fit) {
        let at = HEADER_SIZE + index * KEY_PTR_SIZE;
        let pointer = KeyPtr::parse(&bytes[at..]).expect("the pointer lies inside the block");
        if pointer.blockptr == 0 {
            faults.push(BlockFault::Malformed(format!("child {index} is null")));
            continue;
        }
        if pointers
            .last()
            .is_some_and(|previous| compare_keys(&previous.key, &pointer.key) != Ordering::Less)
        {
            out_of_order = true;
        }
        pointers.push(pointer);
    }
    if out_of_order {
        faults.push(BlockFault::Malformed("child keys do not ascend".into()));
    }
    pointers
}

/// A block from bytes already read, as copy 0 of `logical`.
pub fn salvaged_from_bytes(bytes: Vec<u8>, logical: u64, geometry: &TreeGeometry) -> SalvagedBlock {
    let (header, faults, body) = decode_block(&bytes, logical, geometry, Expect::default());
    SalvagedBlock {
        logical,
        mirror: 0,
        header,
        attempts: vec![CopyAttempt {
            mirror: 0,
            faults: faults.clone(),
        }],
        faults,
        body,
        bytes,
    }
}

/// Read the block at `logical`, trying each copy until one verifies.
///
/// When none does, the copy returned is the first whose header was read,
/// so its fields and items are there to salvage; every copy's faults are in
/// [`SalvagedBlock::attempts`].
pub fn read_block(
    source: &dyn BlockRead,
    layout: &Layout,
    logical: u64,
    expect: Expect,
) -> SalvagedBlock {
    let nodesize = layout.geometry.nodesize as usize;
    let copies = layout.copies(logical).max(1);
    let mut attempts = Vec::with_capacity(copies);
    let mut best: Option<SalvagedBlock> = None;
    for mirror in 0..copies {
        let mut bytes = vec![0u8; nodesize];
        let block = match layout.read_logical(source, logical, mirror, &mut bytes) {
            Err(fault) => SalvagedBlock {
                logical,
                mirror,
                header: None,
                faults: vec![fault],
                body: SalvagedBody::None,
                attempts: Vec::new(),
                bytes: Vec::new(),
            },
            Ok(()) => {
                let (header, faults, body) =
                    decode_block(&bytes, logical, &layout.geometry, expect);
                SalvagedBlock {
                    logical,
                    mirror,
                    header,
                    faults,
                    body,
                    attempts: Vec::new(),
                    bytes,
                }
            }
        };
        attempts.push(CopyAttempt {
            mirror,
            faults: block.faults.clone(),
        });
        let verified = block.is_verified();
        let better = match &best {
            None => true,
            Some(current) => current.header.is_none() && block.header.is_some(),
        };
        if verified || better {
            best = Some(block);
        }
        if verified {
            break;
        }
    }
    let mut block = best.expect("at least one copy is tried");
    block.attempts = attempts;
    block
}

/// Where a tree starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RootPointer {
    pub bytenr: u64,
    /// The generation the root's referrer recorded, when it recorded one.
    pub generation: Option<u64>,
    /// The level the referrer recorded.
    pub level: Option<u8>,
}

/// A block a walk found damaged, and what was wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DamagedBlock {
    pub logical: u64,
    /// The generation the parent recorded, if any.
    pub expected_generation: Option<u64>,
    /// The faults of the copy the walk used.
    pub faults: Vec<BlockFault>,
    /// Every copy's faults.
    pub attempts: Vec<CopyAttempt>,
    /// Whether the walk could still use the block's body.
    pub decoded: bool,
}

/// The outcome of a [`walk_tree`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkReport {
    /// Blocks read, damaged or not.
    pub blocks: u64,
    /// Leaves handed to the visitor.
    pub leaves: u64,
    /// Every block with a fault, in the order the walk met them. A block
    /// that verified only on a later copy is here too: its first copy is
    /// damage.
    pub damaged: Vec<DamagedBlock>,
    /// Child pointers not followed because the walk had been to that
    /// address already: a cycle, or two parents claiming one child.
    pub revisits: u64,
    /// Set when the visitor stopped the walk.
    pub stopped: bool,
}

/// Walk the tree under `root`, handing every leaf to `visit`, damaged or
/// not.
///
/// A damaged block is recorded and the walk goes on. A node whose body
/// still decodes is descended even when its checksum fails, because its
/// children verify on their own; one whose body does not decode ends that
/// subtree only. Leaves are handed over in key order as far as the nodes
/// above them are in order. Each address is read at most once, so a cycle
/// or a shared child cannot make the walk loop.
pub fn walk_tree(
    source: &dyn BlockRead,
    layout: &Layout,
    root: RootPointer,
    visit: &mut dyn FnMut(&SalvagedBlock) -> ControlFlow<()>,
) -> WalkReport {
    let mut report = WalkReport::default();
    let mut seen = HashSet::new();
    let mut stack = vec![(
        root.bytenr,
        Expect {
            generation: root.generation,
            level: root.level,
        },
    )];
    while let Some((logical, expect)) = stack.pop() {
        if !seen.insert(logical) {
            report.revisits += 1;
            continue;
        }
        let block = read_block(source, layout, logical, expect);
        report.blocks += 1;
        let decoded = !matches!(block.body, SalvagedBody::None);
        if !block.is_verified() || block.attempts.len() > 1 {
            report.damaged.push(DamagedBlock {
                logical,
                expected_generation: expect.generation,
                faults: block.faults.clone(),
                attempts: block.attempts.clone(),
                decoded,
            });
        }
        match &block.body {
            SalvagedBody::None => {}
            SalvagedBody::Leaf(_) => {
                report.leaves += 1;
                if visit(&block).is_break() {
                    report.stopped = true;
                    return report;
                }
            }
            SalvagedBody::Node(pointers) => {
                let level = block.header.as_ref().map_or(1, |h| h.level);
                for pointer in pointers.iter().rev() {
                    stack.push((
                        pointer.blockptr,
                        Expect {
                            generation: Some(pointer.generation),
                            level: level.checked_sub(1),
                        },
                    ));
                }
            }
        }
    }
    report
}
