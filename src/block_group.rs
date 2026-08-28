//! Where a new block can go: block groups, and what is free inside them.
//!
//! A copy-on-write write cannot begin until it knows an address. The
//! encoders in [`crate::tree_write`] produce a block's bytes and
//! [`crate::super_write`] commits a root, but neither decides *where* —
//! and picking wrong does not fail loudly. Writing a tree block over a
//! live extent produces a filesystem that mounts, reads correctly for a
//! while, and returns another file's bytes later.
//!
//! # Two independent answers, which is the point
//!
//! Btrfs records allocation twice, from opposite directions:
//!
//! - the **extent tree** records what is *taken* — one item per
//!   allocated run;
//! - the **free-space tree** records what is *free* — the complement,
//!   maintained by the kernel as a cache.
//!
//! Either can answer "what is free in this block group". This module
//! implements both, which is deliberate: they are derived from different
//! items written at different times, so requiring them to agree on a
//! real filesystem is a check no single-source implementation can make
//! of itself. [`tests/free_space_oracle.rs`] does exactly that.
//!
//! The extent tree is the authority. The free-space tree is a cache with
//! a validity bit, and a filesystem may not have one at all.
//!
//! # The trap in reading allocated extents
//!
//! Under `SKINNY_METADATA` — on by default for years — a tree block is
//! recorded as a [`key_type::METADATA_ITEM`] whose key `offset` is the
//! block's **level**, not its length. The length is `nodesize`. Read
//! naively alongside [`key_type::EXTENT_ITEM`], whose `offset` really is
//! a length, it yields extents of length 0, 1 and 2, and every tree
//! block on the filesystem reads as free.

use crate::btree::Tree;
use crate::chunk::{block_group as flags, key_type, objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::Filesystem;

/// Byte offsets within `struct btrfs_block_group_item`.
mod block_group_item {
    /// Bytes in use within the group. Their sum over every group is the
    /// superblock's `bytes_used`.
    pub const USED: usize = 0;
    /// The chunk this group is backed by.
    #[allow(dead_code)]
    pub const CHUNK_OBJECTID: usize = 8;
    /// What the group may hold — data, metadata, system, and the
    /// replication profile.
    pub const FLAGS: usize = 16;
    /// The whole item.
    pub const SIZE: usize = 24;
}

/// Byte offsets within `struct btrfs_free_space_info`.
mod free_space_info {
    /// How many free-space items this group has.
    pub const EXTENT_COUNT: usize = 0;
    /// Bit 0 says the group's free space is recorded as bitmaps rather
    /// than as extents.
    pub const FLAGS: usize = 4;
    pub const SIZE: usize = 8;
}

/// `BTRFS_FREE_SPACE_USING_BITMAPS`.
const USING_BITMAPS: u32 = 1;

/// One allocation region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockGroup {
    /// First byte of the region, in logical address space.
    pub start: u64,
    /// Its length in bytes.
    pub length: u64,
    /// Bytes allocated within it.
    pub used: u64,
    /// What it may hold, and how it is replicated.
    pub flags: u64,
}

impl BlockGroup {
    /// One past the last byte.
    pub fn end(&self) -> u64 {
        self.start + self.length
    }

    /// Whether this group may hold tree blocks.
    ///
    /// True for a mixed group as well, which really does take both — so
    /// this is a test of the bit, not of the group being metadata-only.
    pub fn holds_metadata(&self) -> bool {
        self.flags & flags::METADATA != 0
    }

    /// Whether the group holds file data.
    pub fn holds_data(&self) -> bool {
        self.flags & flags::DATA != 0
    }

    /// Bytes not allocated. Derived, and worth comparing against a
    /// free-extent walk: they should sum to this.
    pub fn free(&self) -> u64 {
        self.length.saturating_sub(self.used)
    }
}

/// A run of free bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FreeExtent {
    pub start: u64,
    pub len: u64,
}

impl FreeExtent {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

impl Filesystem {
    /// Every block group on the filesystem, in address order.
    ///
    /// # Errors
    ///
    /// Propagates a tree read failure. An empty result is not an error
    /// here but is close to impossible — a filesystem with no block
    /// group has nowhere for its own root tree to live.
    pub fn block_groups(&self) -> Result<Vec<BlockGroup>> {
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical(&self.device, &self.map, logical, buf)
        };
        let tree = Tree::from_superblock(&self.sb, &read);

        let mut out = Vec::new();
        tree.for_each(root, &mut |key: &DiskKey, data: &[u8]| {
            if key.key_type == key_type::BLOCK_GROUP_ITEM && data.len() >= block_group_item::SIZE {
                out.push(BlockGroup {
                    start: key.objectid,
                    // The length is in the key, not the item — the item
                    // holds only usage and flags.
                    length: key.offset,
                    used: le64(data, block_group_item::USED),
                    flags: le64(data, block_group_item::FLAGS),
                });
            }
            Ok(true)
        })?;
        out.sort_by_key(|g| g.start);
        Ok(out)
    }

    /// What is free in `group`, worked out from what the extent tree
    /// says is taken.
    ///
    /// This is the authoritative answer: the extent tree is what the
    /// filesystem is, where the free-space tree is a cache of the
    /// complement.
    ///
    /// # Errors
    ///
    /// Propagates a tree read failure, and reports an allocated extent
    /// that overlaps another rather than silently merging them — two
    /// items claiming the same bytes is corruption, and an allocator
    /// that smoothed over it would hand out one of those bytes again.
    pub fn free_extents(&self, group: &BlockGroup) -> Result<Vec<FreeExtent>> {
        let mut taken = self.allocated_extents(group)?;
        taken.sort_by_key(|e| e.start);
        Self::gaps(group, &taken)
    }

    /// What is free in every group, from ONE walk of the extent tree.
    ///
    /// [`Filesystem::free_extents`] answers for a single group and walks
    /// the whole extent tree to do it, because an allocated run for any
    /// group can be filed anywhere in it. A caller asking about every
    /// group therefore walked it once per group — on a filesystem with
    /// a dozen groups that is a dozen full traversals for one answer,
    /// and it made the allocation suite take five minutes.
    ///
    /// Returns one list per group, in the order given.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::free_extents`].
    pub fn free_extents_by_group(&self, groups: &[BlockGroup]) -> Result<Vec<Vec<FreeExtent>>> {
        let mut taken = self.allocated_by_group(groups)?;
        let mut out = Vec::with_capacity(groups.len());
        for (group, runs) in groups.iter().zip(taken.iter_mut()) {
            runs.sort_by_key(|e| e.start);
            out.push(Self::gaps(group, runs)?);
        }
        Ok(out)
    }

    /// The runs of `group` not covered by anything in `taken`, which
    /// must be sorted.
    fn gaps(group: &BlockGroup, taken: &[FreeExtent]) -> Result<Vec<FreeExtent>> {
        let mut free = Vec::new();
        let mut pos = group.start;
        for e in taken {
            if e.start < pos {
                return Err(Error::UnsupportedFeature(format!(
                    "the extent tree has an allocated run at {} that overlaps the one \
                     ending at {pos}, in the block group at {}",
                    e.start, group.start
                )));
            }
            if e.start > pos {
                free.push(FreeExtent {
                    start: pos,
                    len: e.start - pos,
                });
            }
            pos = e.end();
        }
        if pos < group.end() {
            free.push(FreeExtent {
                start: pos,
                len: group.end() - pos,
            });
        }
        Ok(free)
    }

    /// The allocated runs inside `group`, from the extent tree.
    fn allocated_extents(&self, group: &BlockGroup) -> Result<Vec<FreeExtent>> {
        Ok(self
            .allocated_by_group(std::slice::from_ref(group))?
            .pop()
            .unwrap_or_default())
    }

    /// The allocated runs of every group, from one traversal.
    fn allocated_by_group(&self, groups: &[BlockGroup]) -> Result<Vec<Vec<FreeExtent>>> {
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical(&self.device, &self.map, logical, buf)
        };
        let tree = Tree::from_superblock(&self.sb, &read);
        let nodesize = self.sb.nodesize as u64;

        let mut out = vec![Vec::new(); groups.len()];
        tree.for_each(root, &mut |key: &DiskKey, _data: &[u8]| {
            // Reference items (TREE_BLOCK_REF and friends) are filed
            // under the same objectid as the extent they describe. They
            // are not allocations of their own and adding them would
            // double-count every extent that has one.
            let len = match key.key_type {
                key_type::EXTENT_ITEM => key.offset,
                // SKINNY_METADATA: the offset is a level. See the
                // module docs — this is the one that silently frees
                // every tree block if read as a length.
                key_type::METADATA_ITEM => nodesize,
                _ => return Ok(true),
            };
            // Groups do not overlap, so at most one claims this run.
            if let Some(i) = groups
                .iter()
                .position(|g| key.objectid >= g.start && key.objectid < g.end())
            {
                out[i].push(FreeExtent {
                    start: key.objectid,
                    len,
                });
            }
            Ok(true)
        })?;
        Ok(out)
    }

    /// What the kernel's free-space tree says is free in `group`.
    ///
    /// Returns `None` when the filesystem has no free-space tree, which
    /// is a legitimate configuration and not an error.
    ///
    /// This is a *cache*. It is read here to be compared against
    /// [`Filesystem::free_extents`], not to be trusted in its place.
    ///
    /// # Errors
    ///
    /// Propagates a tree read failure, and reports a group whose
    /// free-space info is missing while the tree exists — that is a
    /// cache that has lost a group rather than a group with no free
    /// space, and the two must not read the same.
    pub fn cached_free_extents(&self, group: &BlockGroup) -> Result<Option<Vec<FreeExtent>>> {
        let Ok(root) = self.tree_root(objectid::FREE_SPACE_TREE) else {
            return Ok(None);
        };
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical(&self.device, &self.map, logical, buf)
        };
        let tree = Tree::from_superblock(&self.sb, &read);
        let sectorsize = self.sb.sectorsize as u64;

        let mut info = None;
        let mut out: Vec<FreeExtent> = Vec::new();
        let mut err = None;

        tree.for_each(root, &mut |key: &DiskKey, data: &[u8]| {
            if key.objectid < group.start || key.objectid >= group.end() {
                return Ok(true);
            }
            match key.key_type {
                key_type::FREE_SPACE_INFO if data.len() >= free_space_info::SIZE => {
                    info = Some((
                        le32(data, free_space_info::EXTENT_COUNT),
                        le32(data, free_space_info::FLAGS),
                    ));
                }
                // The key IS the item: a start and a length, no body.
                key_type::FREE_SPACE_EXTENT => out.push(FreeExtent {
                    start: key.objectid,
                    len: key.offset,
                }),
                key_type::FREE_SPACE_BITMAP => {
                    // One bit per sector, least significant bit first,
                    // covering `key.offset` bytes from `key.objectid`.
                    let sectors = (key.offset / sectorsize) as usize;
                    if data.len() * 8 < sectors {
                        err = Some(format!(
                            "a free-space bitmap at {} covers {} bytes, which needs {sectors} \
                             bits, but the item holds {}",
                            key.objectid,
                            key.offset,
                            data.len() * 8
                        ));
                        return Ok(false);
                    }
                    let mut run: Option<u64> = None;
                    for i in 0..sectors {
                        let free = data[i / 8] & (1 << (i % 8)) != 0;
                        let at = key.objectid + i as u64 * sectorsize;
                        match (free, run) {
                            (true, None) => run = Some(at),
                            (false, Some(from)) => {
                                out.push(FreeExtent {
                                    start: from,
                                    len: at - from,
                                });
                                run = None;
                            }
                            _ => {}
                        }
                    }
                    if let Some(from) = run {
                        let end = key.objectid + key.offset;
                        out.push(FreeExtent {
                            start: from,
                            len: end - from,
                        });
                    }
                }
                _ => {}
            }
            Ok(true)
        })?;

        if let Some(msg) = err {
            return Err(Error::UnsupportedFeature(msg));
        }

        // A group the cache has no entry for is not a group with no free
        // space. Saying so is the difference between "full" and "not
        // recorded", and an allocator must not read the second as the
        // first.
        let Some((_count, info_flags)) = info else {
            return Err(Error::UnsupportedFeature(format!(
                "the free-space tree holds no info item for the block group at {}, so what \
                 it believes is free there cannot be established",
                group.start
            )));
        };
        let _ = info_flags & USING_BITMAPS;

        out.sort();
        // Adjacent runs come out of a bitmap already merged, but a
        // bitmap item and an extent item can abut across a boundary.
        Ok(Some(merge_adjacent(out)))
    }

    /// Walk the extent tree, handing every item to `visit`.
    ///
    /// The extent tree is what the filesystem believes is allocated, so
    /// this is how anything that must agree with that belief -- an
    /// allocator, a checker, a test rebuilding the kernel's own items --
    /// gets to see it.
    ///
    /// # Errors
    ///
    /// Propagates a tree read failure.
    pub fn for_each_extent_item(&self, visit: &mut dyn FnMut(&DiskKey, &[u8])) -> Result<()> {
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical(&self.device, &self.map, logical, buf)
        };
        let tree = Tree::from_superblock(&self.sb, &read);
        tree.for_each(root, &mut |key: &DiskKey, data: &[u8]| {
            visit(key, data);
            Ok(true)
        })
    }

    /// Look up a tree's root address by its objectid.
    ///
    /// # Errors
    ///
    /// [`Error::BadSuperblock`] when the root tree holds no `ROOT_ITEM`
    /// for `objectid` — for an optional tree, that is how a caller
    /// learns the tree is absent.
    pub(crate) fn tree_root(&self, objectid: u64) -> Result<u64> {
        const ROOT_ITEM_KEY: u8 = 132;
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical(&self.device, &self.map, logical, buf)
        };
        let tree = Tree::from_superblock(&self.sb, &read);
        let mut root = None;
        tree.for_each(self.sb.root, &mut |key: &DiskKey, data: &[u8]| {
            if key.objectid == objectid
                && key.key_type == ROOT_ITEM_KEY
                && data.len() > crate::fs::root_item::BYTENR + 8
            {
                root = Some(le64(data, crate::fs::root_item::BYTENR));
                return Ok(false);
            }
            Ok(true)
        })?;
        root.ok_or_else(|| {
            Error::BadSuperblock(format!(
                "the root tree holds no ROOT_ITEM for tree {objectid}"
            ))
        })
    }

    /// Find somewhere to put one new tree block.
    ///
    /// First fit across the metadata block groups, aligned to
    /// `nodesize` — a tree block that straddles that alignment is one
    /// the kernel will not read back.
    ///
    /// # What this does not do
    ///
    /// It does not *record* the allocation. The extent tree still says
    /// the returned address is free, and calling this twice without a
    /// transaction in between returns the same address twice. Recording
    /// an allocation means adding a `METADATA_ITEM` and its back
    /// reference and moving the group's `used`, which is the next piece
    /// of work and not this one.
    ///
    /// It also does not allocate a new block group when every existing
    /// one is full; it reports that instead.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] when no metadata block group has a
    /// free run long enough, naming how much was free — a caller that
    /// needs to grow the filesystem needs to know it is full, not that
    /// something went wrong.
    pub fn find_metadata_block(&self) -> Result<u64> {
        let nodesize = self.sb.nodesize as u64;
        let mut best_free = 0u64;

        // One walk of the extent tree for every group, rather than one
        // per group.
        let groups: Vec<BlockGroup> = self
            .block_groups()?
            .into_iter()
            .filter(|g| g.holds_metadata())
            .collect();
        for runs in self.free_extents_by_group(&groups)? {
            for run in runs {
                best_free = best_free.max(usable_in(run, nodesize));
                if let Some(at) = place_in_run(run, nodesize) {
                    return Ok(at);
                }
            }
        }

        Err(Error::UnsupportedFeature(format!(
            "no metadata block group has {nodesize} contiguous bytes aligned to a tree \
             block; the longest usable run is {best_free}. Allocating a new block group \
             is not implemented"
        )))
    }
}

/// How much of a free run is usable once alignment is paid for.
///
/// A run starting mid-block loses the remainder of that block, so the
/// usable length is not the run's length.
fn usable_in(run: FreeExtent, nodesize: u64) -> u64 {
    run.end()
        .saturating_sub(run.start.next_multiple_of(nodesize))
}

/// Where in a free run a tree block goes, if it fits.
///
/// Separated from the search over block groups so that alignment can be
/// tested on a run that is actually misaligned. On a real filesystem
/// every free run happens to start aligned already, so a version of this
/// with the alignment removed passes against every fixture — the
/// mutation survived, which is what prompted pulling it out.
fn place_in_run(run: FreeExtent, nodesize: u64) -> Option<u64> {
    let start = run.start.next_multiple_of(nodesize);
    // Computed from the aligned start, not from the run's length:
    // a 5-byte run at offset 3 of a 4-byte block has 4 bytes of length
    // and nowhere to put anything.
    if run.end().saturating_sub(start) >= nodesize {
        Some(start)
    } else {
        None
    }
}

/// Join runs that touch, so two sources of the same free space compare
/// equal regardless of how each happened to split it.
fn merge_adjacent(runs: Vec<FreeExtent>) -> Vec<FreeExtent> {
    let mut out: Vec<FreeExtent> = Vec::with_capacity(runs.len());
    for run in runs {
        match out.last_mut() {
            Some(prev) if prev.end() == run.start => prev.len += run.len,
            _ => out.push(run),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(start: u64, len: u64) -> FreeExtent {
        FreeExtent { start, len }
    }

    /// Runs that touch become one; runs with a gap do not.
    #[test]
    fn adjacent_runs_merge_and_separated_ones_do_not() {
        assert_eq!(
            merge_adjacent(vec![run(0, 10), run(10, 5), run(20, 5)]),
            vec![run(0, 15), run(20, 5)]
        );
        assert_eq!(merge_adjacent(vec![]), vec![]);
        assert_eq!(merge_adjacent(vec![run(4096, 8192)]), vec![run(4096, 8192)]);
    }

    /// A group's own accounting.
    #[test]
    fn a_group_reports_its_end_and_what_is_free() {
        let g = BlockGroup {
            start: 22_020_096,
            length: 8_388_608,
            used: 1_048_576,
            flags: flags::METADATA | flags::DUP,
        };
        assert_eq!(g.end(), 22_020_096 + 8_388_608);
        assert_eq!(g.free(), 8_388_608 - 1_048_576);
        assert!(g.holds_metadata());
        assert!(!g.holds_data());
    }

    /// A mixed group holds both, so the two tests must not be
    /// exclusive.
    #[test]
    fn a_mixed_group_holds_data_and_metadata() {
        let g = BlockGroup {
            start: 0,
            length: 1024,
            used: 0,
            flags: flags::DATA | flags::METADATA,
        };
        assert!(g.holds_metadata());
        assert!(g.holds_data());
    }

    /// A block goes at the first aligned address in the run, not at the
    /// run's start.
    #[test]
    fn a_misaligned_run_places_the_block_at_the_next_boundary() {
        // Starts 1000 bytes into a 4096-byte block, and is long enough
        // to hold one once that is paid for.
        assert_eq!(place_in_run(run(1000, 4096 + 3096), 4096), Some(4096));
        assert_eq!(place_in_run(run(0, 4096), 4096), Some(0));
        assert_eq!(place_in_run(run(8192, 40960), 4096), Some(8192));
    }

    /// A run long enough on paper but not after alignment is refused.
    ///
    /// This is the case that makes the difference between measuring
    /// against the run's length and measuring against its aligned start:
    /// 4096 bytes of free space that straddle a boundary hold no block
    /// at all.
    #[test]
    fn a_run_that_only_fits_before_alignment_is_refused() {
        assert_eq!(place_in_run(run(1000, 4096), 4096), None);
        // 1000 bytes do remain past the boundary — 1000..5096 aligns up
        // to 4096 and ends at 5096. They are simply not a whole block,
        // which is the distinction `usable_in` reports and
        // `place_in_run` acts on.
        assert_eq!(usable_in(run(1000, 4096), 4096), 1000);
        assert_eq!(place_in_run(run(0, 4095), 4096), None);
        assert_eq!(place_in_run(run(0, 0), 4096), None);
    }

    /// Usable length is measured from the aligned start.
    #[test]
    fn usable_length_pays_for_alignment_first() {
        assert_eq!(usable_in(run(0, 8192), 4096), 8192);
        assert_eq!(usable_in(run(4095, 8193), 4096), 8192, "loses one byte");
        assert_eq!(usable_in(run(1, 10), 4096), 0, "nowhere near a boundary");
    }

    /// `used` above `length` would be corruption; free saturates rather
    /// than wrapping to something near u64::MAX, which an allocator
    /// would read as "plenty of room".
    #[test]
    fn free_space_saturates_rather_than_wrapping() {
        let g = BlockGroup {
            start: 0,
            length: 1024,
            used: 4096,
            flags: flags::METADATA,
        };
        assert_eq!(g.free(), 0);
    }
}
