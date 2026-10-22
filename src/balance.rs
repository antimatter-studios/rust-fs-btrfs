//! Moving a block group's contents somewhere else: balance (#304).
//!
//! Relocating a block group starts from knowing what is in it: every data
//! extent, and every `EXTENT_DATA` item that has to be pointed somewhere
//! else once the extent moves (#396). [`Filesystem::block_group_extents`]
//! answers that for a data group.
//!
//! An extent is found by its `EXTENT_ITEM` in the extent tree, and the
//! items naming it through its back references: each keyed data
//! reference, inline in the extent item or an item of its own, names a
//! tree, an inode, the file offset of the extent's first byte and how
//! many of that inode's items name the extent there. The items are looked
//! up in that tree, and the extent's own reference count must equal what
//! its references add up to, and each reference's count what the tree
//! holds. Nothing is guessed: a reference that cannot be followed, or a
//! count that does not add up, is refused rather than skipped, because a
//! relocation that missed an item would leave it pointing at freed space.
//!
//! # What data enumeration refuses
//!
//! - a group that holds **tree blocks**, or is not a data group at all;
//! - a **shared** data reference, one naming the tree block that holds
//!   the pointer rather than the tree, which snapshots and the kernel's
//!   own balance leave;
//! - a volume where a subvolume **has been snapshotted**: a tree block the
//!   snapshot shares holds items naming extents, and the snapshot's tree
//!   is recorded by no reference, so the items it names would be missed;
//! - references whose counts do not add up to the extent's, or name file
//!   items that are not there.
//!
//! # Relocating a group
//!
//! [`Filesystem::relocate_block_group`] moves a data or metadata group
//! (#397, #493, #580) in committed transactions, each leaving a filesystem the kernel and
//! `btrfs check` accept on its own:
//!
//! 1. **A new chunk** of the group's profile, or DUP when a single data
//!    group is converted (#586): its `CHUNK_ITEM` in the
//!    chunk tree, a `DEV_EXTENT` in the device tree for each stripe, an
//!    empty `BLOCK_GROUP_ITEM`, a `FREE_SPACE_INFO` and one free run
//!    covering it in the free-space tree, and the device's `bytes_used`
//!    raised in the chunk tree's device item and the superblock's copy.
//! 2. **The extents move**, when the group holds any (#493), at most 64
//!    per committed transaction (#494): each one's bytes are read verified
//!    against their digests and written into every copy the new chunk keeps, then the
//!    batch's commit re-points every file item naming it (#495), each
//!    keeping its offset into the extent, moves its `EXTENT_ITEM` and
//!    inline and keyed back references unchanged, files its
//!    checksums under the new sectors, and carries the free-space tree and
//!    both groups' `used` with it.
//!    For metadata, every tree block is copied out of the old group in
//!    one transaction (#578), with the old group excluded from allocation.
//! 3. **The old group goes**: its chunk item, device extents, block group
//!    item and free-space records, and `bytes_used` lowered again. Metadata
//!    allocation still excludes it during this final transaction.
//!
//! Besides everything above, it refuses a system group, a conversion to
//! another profile but a single data group's to DUP, a group of any
//! profile but single, a volume of more than one
//! device, one with no free-space tree or with a block group tree, and a
//! group holding an extent with a sector that a checksummed file's
//! regular item names and that has no checksum, each before anything is
//! written. Preallocated extents and those of `NODATASUM` files move with
//! the checksums they have and no others (#496). A batch
//! that fails after an earlier one committed leaves both groups, every
//! extent in exactly one of them.
//! Metadata relocation also refuses mixed data and metadata groups,
//! shared tree blocks and a log tree awaiting replay before allocating
//! the replacement chunk (#579).

use crate::block_group::BlockGroup;
use crate::chunk::{
    block_group as bg, key_type, objectid, ChunkProfile, DiskKey, DEFAULT_STRIPE_LEN,
};
use crate::error::{Error, Result};
use crate::fs::{
    file_extent as fe, root_item, Filesystem, EXTENT_DATA_KEY, EXTENT_PREALLOC, EXTENT_REGULAR,
    ROOT_ITEM_KEY,
};
use crate::inode::{Inode, INODE_ITEM_KEY, INODE_NODATASUM};
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::transaction::{DataWrite, ItemEdit, RecordEdit};

/// How many rounds a chunk change's plan has to close over its own
/// bookkeeping.
const PLAN_ROUNDS: usize = 64;

/// Where a new chunk's stripes start and how they are aligned: the
/// kernel leaves the first MiB of every device alone, and places device
/// extents on MiB boundaries.
const MIB: u64 = 1 << 20;

/// The most data extents one relocation transaction moves (#493): a
/// group holding more is moved a batch of this many at a time, each batch
/// committed before the next is copied (#494).
const EXTENTS_PER_TRANSACTION: usize = 64;

/// How much of an extent is read and written at a time while it is copied.
const COPY_PIECE: u64 = MIB;

/// `BTRFS_DEV_EXTENT_KEY`.
const DEV_EXTENT_KEY: u8 = 204;

/// Byte offsets within `struct btrfs_dev_extent`.
mod dev_extent {
    /// The chunk's logical address.
    pub const CHUNK_OFFSET: usize = 16;
    /// The stripe's length on the device.
    pub const LENGTH: usize = 24;
    /// The chunk tree's UUID.
    pub const CHUNK_TREE_UUID: usize = 32;
    /// The whole item.
    pub const SIZE: usize = 48;
}

/// Where a device item's size is, in the chunk tree's copy.
const DEV_ITEM_TOTAL_BYTES: usize = 8;
/// Where a device item's `bytes_used` is, in the chunk tree's copy.
const DEV_ITEM_BYTES_USED: usize = 16;
/// A `BLOCK_GROUP_ITEM`: used, chunk objectid, flags.
const BLOCK_GROUP_ITEM_SIZE: usize = 24;

/// `BTRFS_EXTENT_DATA_REF_KEY`, keyed or inline.
const EXTENT_DATA_REF: u8 = crate::backref::EXTENT_DATA_REF;
/// `BTRFS_SHARED_DATA_REF_KEY`, keyed or inline.
const SHARED_DATA_REF: u8 = crate::backref::SHARED_DATA_REF;
/// A keyed data reference: the tree, the inode, the file offset of the
/// extent's first byte, and a count.
const DATA_REF_SIZE: usize = 28;

/// Which block groups a balance relocates, and what it makes of them:
/// the kernel's balance filters, for the ones it implements (#584).
#[derive(Debug, Clone, Default)]
pub struct Balance {
    /// What the groups hold, as [`crate::chunk::block_group`] bits: a
    /// group holding any of them is a candidate.
    pub kinds: u64,
    /// Only groups less than this many per cent used, as the kernel's
    /// `usage=` filter; `Some(0)` takes only empty ones.
    pub usage: Option<u8>,
    /// Only groups in one of these profiles, as the kernel's `profiles=`.
    pub profiles: Option<Vec<ChunkProfile>>,
    /// The profile each relocated group's new chunk is given; `None` keeps
    /// the group's own. A group already in it is left where it is, as the
    /// kernel's `soft` modifier leaves it.
    pub convert: Option<ChunkProfile>,
}

impl Balance {
    /// Whether `group` is one this balance relocates.
    ///
    /// # Errors
    ///
    /// [`Error::BadChunkItem`] when the group's flags set more than one
    /// profile bit, as [`ChunkProfile::from_type`] refuses them.
    pub fn selects(&self, group: &BlockGroup) -> Result<bool> {
        if group.flags & self.kinds & bg::TYPE_MASK == 0 {
            return Ok(false);
        }
        let profile = ChunkProfile::from_type(group.flags)?;
        if let Some(wanted) = &self.profiles {
            if !wanted.contains(&profile) {
                return Ok(false);
            }
        }
        if self.convert == Some(profile) {
            return Ok(false);
        }
        if let Some(usage) = self.usage {
            // The kernel's threshold: a share of the group's length, and
            // one byte for a usage of zero, so that it takes empty groups.
            let threshold = match usage {
                0 => 1,
                u => (u128::from(group.length) * u128::from(u.min(100)) / 100) as u64,
            };
            if group.used >= threshold {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The profile bits of a chunk `type` for `profile`: none for single.
pub fn profile_bits(profile: ChunkProfile) -> u64 {
    match profile {
        ChunkProfile::Single => 0,
        ChunkProfile::Dup => bg::DUP,
        ChunkProfile::Raid0 => bg::RAID0,
        ChunkProfile::Raid1 => bg::RAID1,
        ChunkProfile::Raid1c3 => bg::RAID1C3,
        ChunkProfile::Raid1c4 => bg::RAID1C4,
        ChunkProfile::Raid10 => bg::RAID10,
        ChunkProfile::Raid5 => bg::RAID5,
        ChunkProfile::Raid6 => bg::RAID6,
    }
}

/// One data extent of a block group, and every file item naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupExtent {
    /// Where it starts, in logical address space.
    pub start: u64,
    /// Its length on disk, as its `EXTENT_ITEM` key records it.
    pub len: u64,
    /// Every `EXTENT_DATA` item naming it, ordered by tree, inode and
    /// file offset.
    pub items: Vec<FileExtentRef>,
}

/// One `EXTENT_DATA` item naming a data extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileExtentRef {
    /// The tree the item is in: 5 for the top-level subvolume, or the
    /// subvolume's own.
    pub tree: u64,
    /// The inode it belongs to.
    pub inode: u64,
    /// The file offset of its first byte: the item's key offset.
    pub file_offset: u64,
    /// How far into the extent its first byte is: the item's `offset`,
    /// counted in the extent's decompressed bytes when it is compressed.
    pub extent_offset: u64,
    /// How many bytes of the file it covers: the item's `num_bytes`.
    pub len: u64,
}

/// One data extent of a group, and everything that names it, as a
/// relocation will need it: the records move with the extent, and the
/// file items are rewritten to point at its new address.
#[derive(Debug, Clone)]
struct DataExtent {
    /// Where it is.
    at: u64,
    /// Its length on disk, as its `EXTENT_ITEM` key records it.
    len: u64,
    /// Its `EXTENT_ITEM`'s body.
    body: Vec<u8>,
    /// Its back references filed as items of their own, under its
    /// address.
    keyed: Vec<OwnedItem>,
    /// Every file item naming it, with the tree each is in.
    items: Vec<(u64, OwnedItem)>,
}

impl DataExtent {
    fn public(&self) -> GroupExtent {
        let mut items: Vec<FileExtentRef> = self
            .items
            .iter()
            .map(|(tree, item)| FileExtentRef {
                tree: *tree,
                inode: item.key.objectid,
                file_offset: item.key.offset,
                extent_offset: le64(&item.data, fe::OFFSET),
                len: le64(&item.data, fe::NUM_BYTES),
            })
            .collect();
        items.sort_unstable();
        GroupExtent {
            start: self.at,
            len: self.len,
            items,
        }
    }
}

impl Filesystem {
    /// Every data extent of the data block group at `start`, in address
    /// order, each with every file item naming it.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when no block group starts at `start`, and
    /// [`Error::UnsupportedFeature`] for every refusal the [module
    /// documentation](crate::balance) lists.
    pub fn block_group_extents(&self, start: u64) -> Result<Vec<GroupExtent>> {
        let group = self.block_group_at(start)?;
        self.refuse_unenumerable(&group)?;
        Ok(self
            .data_extents_in(&group)?
            .iter()
            .map(DataExtent::public)
            .collect())
    }

    /// Relocate the data or metadata block group at `start` into a new
    /// chunk of its own profile, or of `profile` when that converts a
    /// single data group to DUP, and return where the new group is (#397,
    /// #493, #580, #586).
    ///
    /// Committed transactions, each leaving a filesystem the kernel
    /// accepts on its own: the new chunk is allocated, the group's extents
    /// are moved a batch per transaction (#494), or its tree blocks are
    /// copied out in one transaction (#578), then the old group is removed.
    /// A crash between them leaves both groups, every
    /// extent in exactly one of them.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotFound`] when no block group starts at `start`, and
    /// [`Error::UnsupportedFeature`] for every refusal the [module
    /// documentation](crate::balance) lists, each before anything is
    /// written. An error from a later batch leaves the batches before it
    /// committed and both groups in place.
    /// Metadata tree blocks move in one transaction; a failure after chunk
    /// allocation leaves both groups available for a later retry.
    pub fn relocate_block_group(
        &mut self,
        start: u64,
        profile: Option<ChunkProfile>,
    ) -> Result<u64> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        let group = self.block_group_at(start)?;
        let extents = self.refuse_unrelocatable(&group, profile)?;
        // The new chunk takes the converted profile, if any; the extents
        // move through every mirror of it, so a DUP chunk gets both copies.
        let flags = match profile {
            Some(p) => group.flags & !bg::PROFILE_MASK | profile_bits(p),
            None => group.flags,
        };
        let new = self.allocate_chunk(flags, group.length)?;
        if group.holds_metadata() {
            // Allocation commits while the old metadata is still in use.
            // Only after that can copy-on-write allocate in the new chunk.
            self.copy_tree_blocks_out_of(start)?;
            // Removal itself rewrites bookkeeping trees. Keep their new
            // blocks out of the group being removed, even though the copy
            // transaction remounted and restored its allocator exclusion.
            let was = self.emptying.replace(start);
            let done = self.remove_block_group(start);
            self.emptying = was;
            done?;
            return Ok(new);
        }
        for (i, batch) in extents.chunks(EXTENTS_PER_TRANSACTION).enumerate() {
            if let Err(e) = self.move_data_extents(batch, new) {
                if i == 0 {
                    // Nothing has moved: the new chunk is empty, and taking
                    // it away again leaves the filesystem as it was.
                    self.remove_block_group(new)?;
                }
                // Otherwise the committed batches stay where they are,
                // each extent in exactly one of the two groups.
                return Err(e);
            }
        }
        self.remove_block_group(start)?;
        Ok(new)
    }

    /// Copy every tree block in the metadata group at `start` out of it,
    /// in one committed transaction (#578).
    ///
    /// Each tree block reachable from the superblock whose address lies in
    /// the group is made dirty, with the group kept out of the tree block
    /// allocator (#574), so the commit's copy-on-write rewrites it and its
    /// path to its tree's root somewhere else: parents, `ROOT_ITEM`s and
    /// the superblock's root pointers are re-pointed, each block's
    /// `METADATA_ITEM` moves, and the free-space tree and both groups'
    /// `used` follow, as for any commit. The group itself stays, empty of
    /// tree blocks, for whatever removes it.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotFound`] when no block group starts at `start`, and
    /// [`Error::UnsupportedFeature`] for a group that is not a metadata
    /// group or is a system one, a mixed data and metadata group, a volume
    /// with a log tree to replay, or a group containing a shared tree
    /// block, each before anything is written; and when no other metadata
    /// group has room for the copies.
    pub fn copy_tree_blocks_out_of(&mut self, start: u64) -> Result<()> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        let group = self.block_group_at(start)?;
        let dirty = self.metadata_evacuation_blocks(&group)?;
        if dirty.is_empty() {
            return Ok(());
        }
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let was = self.emptying.replace(start);
        let done = self.commit_dirty_with(&dirty, DataWrite::default(), generation, false);
        // The commit reopens the mount, which forgets the exclusion.
        self.emptying = was;
        done
    }

    /// Validate metadata evacuation without writing, including before a
    /// relocation allocates its replacement chunk, and find its tree blocks.
    fn metadata_evacuation_blocks(&self, group: &BlockGroup) -> Result<Vec<u64>> {
        let start = group.start;
        if !group.holds_metadata() || group.flags & bg::SYSTEM != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "the group at {start} is not a metadata group, so it holds no tree blocks \
                 to copy out"
            )));
        }
        if group.holds_data() {
            return Err(Error::UnsupportedFeature(format!(
                "the group at {start} holds mixed data and metadata; copying only its \
                 tree blocks cannot empty it"
            )));
        }
        if self.sb.log_root != 0 {
            return Err(Error::UnsupportedFeature(
                "the volume has a log tree to replay before metadata tree blocks can be moved"
                    .into(),
            ));
        }
        // Inspect every extent record in the group, including blocks a
        // snapshot keeps and the older non-skinny EXTENT_ITEM form. A
        // copy-on-write would leave a shared block behind, not empty it.
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        let from = DiskKey {
            objectid: start,
            key_type: 0,
            offset: 0,
        };
        self.pool_reader()
            .tree()
            .for_each_from(root, &from, &mut |key, data| {
                if key.objectid >= group.end() {
                    return Ok(false);
                }
                let tree_block = key.key_type == key_type::METADATA_ITEM
                    || (key.key_type == key_type::EXTENT_ITEM
                        && crate::backref::flags(data).is_some_and(|flags| {
                            flags & crate::extent_write::EXTENT_FLAG_TREE_BLOCK != 0
                        }));
                if tree_block {
                    let refs = data.get(..8).ok_or_else(|| {
                        Error::UnsupportedFeature(format!(
                            "the extent record for tree block {} is too short to count references",
                            key.objectid
                        ))
                    })?;
                    let refs = u64::from_le_bytes(refs.try_into().expect("8 bytes"));
                    if refs > 1 {
                        return Err(Error::UnsupportedFeature(format!(
                            "the group at {start} holds a shared tree block at {} with {refs} references",
                            key.objectid
                        )));
                    }
                }
                Ok(true)
            })?;
        self.tree_blocks_in(start..group.end())
    }

    /// Move `extents`, one batch of a group's data extents, into the group
    /// at `to` in one committed transaction (#493, #494).
    ///
    /// Each extent's bytes are read as the file read path reads them,
    /// verified against their digests from whichever copy verifies, and
    /// written to the new address before anything is committed: the new
    /// chunk is free space until then, so a crash before the superblock
    /// write leaves the files reading their old extents. The commit then
    /// re-points every file item naming each extent — reflinks, clones
    /// and a partial overwrite's bookends alike — with its offset into the
    /// extent unchanged, so each reference's tree, inode and file offset
    /// still add up; moves its `EXTENT_ITEM` and keyed back references
    /// unchanged; and files its
    /// checksums under the new sectors; the records' release and
    /// allocation carry the free-space tree and both groups' `used`.
    fn move_data_extents(&mut self, extents: &[DataExtent], to: u64) -> Result<()> {
        let target = self.block_group_at(to)?;
        let lens: Vec<u64> = extents.iter().map(|e| e.len).collect();
        let news = self.find_extents_in(std::slice::from_ref(&target), &lens)?;

        for (e, &new) in extents.iter().zip(&news) {
            let mut done = 0u64;
            while done < e.len {
                let n = (e.len - done).min(COPY_PIECE);
                let mut buf = vec![0u8; n as usize];
                self.read_data_verified(e.at + done, &mut buf, true)?;
                self.write_logical_all_mirrors(new + done, &buf)?;
                done += n;
            }
        }

        // Deletes first, so a leaf is at its smallest before anything is
        // added to it.
        let extent_tree = objectid::EXTENT_TREE;
        let mut deletes: Vec<(u64, ItemEdit)> = Vec::new();
        let mut puts: Vec<(u64, ItemEdit)> = Vec::new();
        let mut records = Vec::new();
        for (e, &new) in extents.iter().zip(&news) {
            records.push(RecordEdit {
                bytenr: e.at,
                len: e.len,
                before: Some(e.body.clone()),
                after: None,
            });
            records.push(RecordEdit {
                bytenr: new,
                len: e.len,
                before: None,
                after: Some(e.body.clone()),
            });
            // A keyed reference's offset is a hash of what it names, not of
            // where the extent is, so it moves as it is.
            for item in &e.keyed {
                deletes.push((extent_tree, ItemEdit::Delete(item.key)));
                puts.push((
                    extent_tree,
                    ItemEdit::Put(OwnedItem {
                        key: DiskKey {
                            objectid: new,
                            ..item.key
                        },
                        data: item.data.clone(),
                    }),
                ));
            }
            for (tree, item) in &e.items {
                let mut data = item.data.clone();
                data[fe::DISK_BYTENR..fe::DISK_BYTENR + 8].copy_from_slice(&new.to_le_bytes());
                puts.push((
                    *tree,
                    ItemEdit::Put(OwnedItem {
                        key: item.key,
                        data,
                    }),
                ));
            }
        }
        let moved: Vec<(u64, u64, u64)> = extents
            .iter()
            .zip(&news)
            .map(|(e, &new)| (e.at, new, e.len))
            .collect();
        let csum_edits = self.csum_move_edits(&moved)?;
        let mut tree_edits = deletes;
        tree_edits.extend(puts);
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let write = DataWrite {
            root: objectid::FS_TREE,
            tree_edits,
            csum_edits,
            records,
            ..Default::default()
        };
        self.commit_write(write, generation)
    }

    /// Refuse a group this does not relocate, before anything is written.
    fn refuse_unrelocatable(
        &self,
        group: &BlockGroup,
        profile: Option<ChunkProfile>,
    ) -> Result<Vec<DataExtent>> {
        if group.flags & bg::SYSTEM != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "the system block group at {} holds the chunk tree, which the superblock's \
                 chunk array also names, and relocating it is not implemented",
                group.start
            )));
        }
        if group.holds_metadata() {
            self.metadata_evacuation_blocks(group)?;
        } else {
            self.refuse_unenumerable(group)?;
        }
        let own = ChunkProfile::from_type(group.flags)?;
        // A single data group converts to DUP (#586); every other
        // conversion is refused.
        if let Some(p) = profile.filter(|&p| p != own) {
            if group.holds_metadata() || own != ChunkProfile::Single || p != ChunkProfile::Dup {
                return Err(Error::UnsupportedFeature(format!(
                    "converting a {} {} block group to {} is not implemented; only a \
                     single data group to dup is",
                    own.name(),
                    if group.holds_metadata() {
                        "metadata"
                    } else {
                        "data"
                    },
                    p.name()
                )));
            }
        }
        if own != ChunkProfile::Single {
            return Err(Error::UnsupportedFeature(format!(
                "allocating a {} chunk is not implemented; only single is",
                own.name()
            )));
        }
        if self.sb.num_devices != 1 {
            return Err(Error::UnsupportedFeature(format!(
                "this filesystem spans {} devices, and placing a chunk's stripes on a pool \
                 is not implemented",
                self.sb.num_devices
            )));
        }
        if self.tree_root(objectid::FREE_SPACE_TREE).is_err() {
            return Err(Error::UnsupportedFeature(
                "this volume has no free-space tree, and the free-space cache that stands in \
                 for it is not maintained here"
                    .into(),
            ));
        }
        if self.sb.has_block_group_tree() {
            return Err(Error::UnsupportedFeature(
                "this volume keeps its block group items in a block group tree, and \
                 allocating a chunk there is not implemented"
                    .into(),
            ));
        }
        if group.holds_metadata() {
            return Ok(Vec::new());
        }
        let extents = self.data_extents_in(group)?;
        for e in &extents {
            self.refuse_missing_digests(e)?;
        }
        if group.used != extents.iter().map(|e| e.len).sum::<u64>() {
            return Err(Error::UnsupportedFeature(format!(
                "the block group at {} records {} bytes used, and its data extents do not \
                 add up to that",
                group.start, group.used
            )));
        }
        Ok(extents)
    }

    /// Refuse an extent with a sector a checksummed item names and the
    /// checksum tree has no digest for (#496).
    ///
    /// A sector may lack a digest only where the kernel leaves it without
    /// one: under a preallocated item, which names space never written, or
    /// an item of a `NODATASUM` inode. Such sectors move with no digest,
    /// so none is invented for them, while a sector a regular item of a
    /// checksummed file names must have its digest to move with it. A
    /// compressed item names its whole extent.
    fn refuse_missing_digests(&self, e: &DataExtent) -> Result<()> {
        let sector = u64::from(self.sb.sectorsize).max(1);
        let digests = self.data_digests(e.at, e.len)?;
        if digests.len() as u64 == e.len / sector {
            return Ok(());
        }
        for (tree, item) in &e.items {
            if item.data[fe::TYPE] == EXTENT_PREALLOC
                || self.inode_flags(*tree, item.key.objectid)? & INODE_NODATASUM != 0
            {
                continue;
            }
            let (from, to) = if item.data[fe::COMPRESSION] != 0 {
                (0, e.len)
            } else {
                let from = le64(&item.data, fe::OFFSET);
                let to = from.saturating_add(le64(&item.data, fe::NUM_BYTES));
                (from / sector * sector, to.div_ceil(sector) * sector)
            };
            let mut at = from;
            while at < to.min(e.len) {
                if !digests.contains_key(&(e.at + at)) {
                    return Err(Error::UnsupportedFeature(format!(
                        "the extent at {} has a sector at {} that inode {} of tree {tree} \
                         names as checksummed data and the checksum tree has no digest for",
                        e.at,
                        e.at + at,
                        item.key.objectid
                    )));
                }
                at += sector;
            }
        }
        Ok(())
    }

    /// The flags of inode `ino` in `tree`.
    fn inode_flags(&self, tree: u64, ino: u64) -> Result<u64> {
        let root = self.tree_root(tree)?;
        let reader = self.pool_reader();
        let from = DiskKey {
            objectid: ino,
            key_type: INODE_ITEM_KEY,
            offset: 0,
        };
        let mut found: Option<Vec<u8>> = None;
        reader
            .tree()
            .for_each_from(root, &from, &mut |k: &DiskKey, d: &[u8]| {
                if k.objectid == ino && k.key_type == INODE_ITEM_KEY {
                    found = Some(d.to_vec());
                }
                Ok(false)
            })?;
        let data = found.ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "inode {ino} of tree {tree} names a data extent and has no inode item"
            ))
        })?;
        Ok(Inode::parse(&data, ino)?.flags)
    }

    /// Allocate a single or DUP chunk of `length` logical bytes and
    /// `flags`, with its empty block group, in one committed transaction,
    /// and return its logical address. DUP takes two stripes on this device.
    fn allocate_chunk(&mut self, flags: u64, length: u64) -> Result<u64> {
        let devid = self.sb.dev_item.devid;
        let copies = match ChunkProfile::from_type(flags)? {
            ChunkProfile::Single => 1,
            ChunkProfile::Dup => 2,
            other => {
                return Err(Error::UnsupportedFeature(format!(
                    "allocating a {} chunk is not implemented",
                    other.name()
                )))
            }
        };
        // After every chunk there is, as the kernel places a new one.
        let logical = self
            .map
            .chunks()
            .iter()
            .map(|c| c.logical_end())
            .max()
            .unwrap_or(0);

        let extents = self.dev_extents()?;
        let taken: Vec<(u64, u64)> = extents
            .iter()
            .filter(|e| e.devid == devid)
            .map(|e| (e.physical, e.length))
            .collect();
        let (dev_item_key, mut dev_item) = self.dev_item(devid)?;
        let total = le64(&dev_item, DEV_ITEM_TOTAL_BYTES);
        let stripes = place_stripes(&taken, total, length, copies).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "device {devid} has no room for {copies} stripe(s) of {length} bytes"
            ))
        })?;
        let chunk_tree_uuid = extents
            .first()
            .map(|e| e.chunk_tree_uuid)
            .ok_or_else(|| Error::UnsupportedFeature("the device tree has no extents".into()))?;
        let used = length
            .checked_mul(copies as u64)
            .and_then(|allocated| le64(&dev_item, DEV_ITEM_BYTES_USED).checked_add(allocated))
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!("device {devid}'s bytes used overflow"))
            })?;
        dev_item[DEV_ITEM_BYTES_USED..DEV_ITEM_BYTES_USED + 8].copy_from_slice(&used.to_le_bytes());

        let dev_uuid = self.sb.dev_item.uuid;
        let stripe_list: Vec<(u64, u64, [u8; 16])> =
            stripes.iter().map(|&p| (devid, p, dev_uuid)).collect();
        let mut tree_edits = vec![
            (
                objectid::CHUNK_TREE,
                ItemEdit::Put(OwnedItem {
                    key: dev_item_key,
                    data: dev_item,
                }),
            ),
            (
                objectid::CHUNK_TREE,
                ItemEdit::Put(OwnedItem {
                    key: DiskKey {
                        objectid: objectid::FIRST_CHUNK_TREE,
                        key_type: key_type::CHUNK_ITEM,
                        offset: logical,
                    },
                    data: chunk_item(length, flags, self.sb.sectorsize, &stripe_list),
                }),
            ),
        ];
        for &physical in &stripes {
            tree_edits.push((
                objectid::DEV_TREE,
                ItemEdit::Put(OwnedItem {
                    key: DiskKey {
                        objectid: devid,
                        key_type: DEV_EXTENT_KEY,
                        offset: physical,
                    },
                    data: dev_extent_item(logical, length, chunk_tree_uuid),
                }),
            ));
        }
        // An empty group: nothing used, in the chunk tree's first chunk
        // objectid, of the chunk's type.
        let mut group = Vec::with_capacity(BLOCK_GROUP_ITEM_SIZE);
        group.extend_from_slice(&0u64.to_le_bytes());
        group.extend_from_slice(&objectid::FIRST_CHUNK_TREE.to_le_bytes());
        group.extend_from_slice(&flags.to_le_bytes());
        tree_edits.push((
            objectid::EXTENT_TREE,
            ItemEdit::Put(OwnedItem {
                key: DiskKey {
                    objectid: logical,
                    key_type: key_type::BLOCK_GROUP_ITEM,
                    offset: length,
                },
                data: group,
            }),
        ));
        // Free, all of it: one run, recorded as an extent.
        let mut info = 1u32.to_le_bytes().to_vec();
        info.extend_from_slice(&0u32.to_le_bytes());
        for (key_type, data) in [
            (key_type::FREE_SPACE_INFO, info),
            (key_type::FREE_SPACE_EXTENT, Vec::new()),
        ] {
            tree_edits.push((
                objectid::FREE_SPACE_TREE,
                ItemEdit::Put(OwnedItem {
                    key: DiskKey {
                        objectid: logical,
                        key_type,
                        offset: length,
                    },
                    data,
                }),
            ));
        }
        self.commit_chunk_change(tree_edits, devid, used)?;
        Ok(logical)
    }

    /// Remove the empty block group at `start` and its chunk, in one
    /// committed transaction.
    fn remove_block_group(&mut self, start: u64) -> Result<()> {
        let group = self.block_group_at(start)?;
        if group.used != 0 || !self.data_extents_in(&group)?.is_empty() {
            return Err(Error::UnsupportedFeature(format!(
                "the block group at {start} still holds {} bytes",
                group.used
            )));
        }
        let chunk = self
            .map
            .chunk_for(start)
            .filter(|c| c.logical == start && c.length == group.length)
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "no chunk maps the block group at {start} exactly"
                ))
            })?
            .clone();
        let devid = self.sb.dev_item.devid;
        let stripes: Vec<DevExtent> = self
            .dev_extents()?
            .into_iter()
            .filter(|e| e.chunk_offset == start)
            .collect();
        if stripes.len() != usize::from(chunk.num_stripes)
            || stripes.iter().any(|s| s.devid != devid)
        {
            return Err(Error::UnsupportedFeature(format!(
                "the chunk at {start} has {} stripes, and the device tree has {} extents for \
                 it on device {devid}",
                chunk.num_stripes,
                stripes.len()
            )));
        }

        let mut tree_edits = vec![(
            objectid::CHUNK_TREE,
            ItemEdit::Delete(DiskKey {
                objectid: objectid::FIRST_CHUNK_TREE,
                key_type: key_type::CHUNK_ITEM,
                offset: start,
            }),
        )];
        let mut freed = 0u64;
        for s in &stripes {
            freed += s.length;
            tree_edits.push((
                objectid::DEV_TREE,
                ItemEdit::Delete(DiskKey {
                    objectid: s.devid,
                    key_type: DEV_EXTENT_KEY,
                    offset: s.physical,
                }),
            ));
        }
        let (key, mut item) = self.dev_item(devid)?;
        let used = le64(&item, DEV_ITEM_BYTES_USED)
            .checked_sub(freed)
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "device {devid} records fewer bytes used than the chunk at {start} takes \
                     on it"
                ))
            })?;
        item[DEV_ITEM_BYTES_USED..DEV_ITEM_BYTES_USED + 8].copy_from_slice(&used.to_le_bytes());
        tree_edits.push((
            objectid::CHUNK_TREE,
            ItemEdit::Put(OwnedItem { key, data: item }),
        ));
        tree_edits.push((
            objectid::EXTENT_TREE,
            ItemEdit::Delete(DiskKey {
                objectid: start,
                key_type: key_type::BLOCK_GROUP_ITEM,
                offset: group.length,
            }),
        ));
        // And everything the free-space tree says about the group.
        for key in self.free_space_keys(&group)? {
            tree_edits.push((objectid::FREE_SPACE_TREE, ItemEdit::Delete(key)));
        }
        self.commit_chunk_change(tree_edits, devid, used)
    }

    /// Plan, render and commit `tree_edits`, with device `devid`'s chunks
    /// taking `bytes_used` in its superblock's device item as in the chunk
    /// tree's, then remount.
    fn commit_chunk_change(
        &mut self,
        tree_edits: Vec<(u64, ItemEdit)>,
        devid: u64,
        bytes_used: u64,
    ) -> Result<()> {
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let write = DataWrite {
            root: objectid::CHUNK_TREE,
            tree_edits,
            ..Default::default()
        };
        let dirty: Vec<u64> = {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let mut leaves = std::collections::BTreeSet::new();
            for (t, edit) in &write.tree_edits {
                leaves.insert(
                    tree.descend(self.edit_root(*t)?, &edit.key())?
                        .header
                        .bytenr,
                );
            }
            leaves.into_iter().collect()
        };
        let plan = self.plan_transaction_closed_with(&dirty, &write, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, &write, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the relocation's plan does not move the root tree".into())
        })?;
        let chunk_root = self.planned_chunk_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the relocation's plan does not move the chunk tree".into())
        })?;
        let sb_bytes_used = match plan.usage_delta(u64::from(self.sb.nodesize)) {
            0 => None,
            d => Some(
                u64::try_from(i128::from(self.sb.bytes_used) + d).map_err(|_| {
                    Error::UnsupportedFeature(format!(
                        "the superblock's {} bytes used cannot move by {d}",
                        self.sb.bytes_used
                    ))
                })?,
            ),
        };
        self.commit_bytes_used(
            &blocks,
            &Commit {
                generation,
                root,
                bytes_used: sb_bytes_used,
                chunk_root: Some(chunk_root),
                chunk_root_generation: Some(generation),
                invalidate_free_space_tree: false,
                ..Default::default()
            },
            devid,
            bytes_used,
        )?;
        *self = self.remount_rw()?;
        Ok(())
    }

    /// The chunk tree's item for device `devid`, and its key.
    fn dev_item(&self, devid: u64) -> Result<(DiskKey, Vec<u8>)> {
        let key = DiskKey {
            objectid: objectid::DEV_ITEMS,
            key_type: key_type::DEV_ITEM,
            offset: devid,
        };
        let item = self
            .pool_reader()
            .tree()
            .search(self.sb.chunk_root, &key)?
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!("the chunk tree has no item for device {devid}"))
            })?
            .data;
        if item.len() < DEV_ITEM_BYTES_USED + 8 {
            return Err(Error::UnsupportedFeature(format!(
                "device {devid}'s item is {} bytes, shorter than a device item",
                item.len()
            )));
        }
        Ok((key, item))
    }

    /// Every device extent the device tree records.
    fn dev_extents(&self) -> Result<Vec<DevExtent>> {
        let root = self.tree_root(objectid::DEV_TREE)?;
        let reader = self.pool_reader();
        let tree = reader.tree();
        let mut out = Vec::new();
        let mut bad = None;
        tree.for_each(root, &mut |k: &DiskKey, d: &[u8]| {
            if k.key_type != DEV_EXTENT_KEY {
                return Ok(true);
            }
            if d.len() < dev_extent::SIZE {
                bad = Some(k.offset);
                return Ok(false);
            }
            let mut chunk_tree_uuid = [0u8; 16];
            chunk_tree_uuid
                .copy_from_slice(&d[dev_extent::CHUNK_TREE_UUID..dev_extent::CHUNK_TREE_UUID + 16]);
            out.push(DevExtent {
                devid: k.objectid,
                physical: k.offset,
                chunk_offset: le64(d, dev_extent::CHUNK_OFFSET),
                length: le64(d, dev_extent::LENGTH),
                chunk_tree_uuid,
            });
            Ok(true)
        })?;
        if let Some(at) = bad {
            return Err(Error::UnsupportedFeature(format!(
                "the device extent at {at} is shorter than a device extent"
            )));
        }
        Ok(out)
    }

    /// The key of every free-space tree item describing `group`.
    fn free_space_keys(&self, group: &BlockGroup) -> Result<Vec<DiskKey>> {
        let root = self.tree_root(objectid::FREE_SPACE_TREE)?;
        let reader = self.pool_reader();
        let tree = reader.tree();
        let mut out = Vec::new();
        let from = DiskKey {
            objectid: group.start,
            key_type: 0,
            offset: 0,
        };
        tree.for_each_from(root, &from, &mut |k: &DiskKey, _| {
            if k.objectid >= group.end() {
                return Ok(false);
            }
            if matches!(
                k.key_type,
                key_type::FREE_SPACE_INFO
                    | key_type::FREE_SPACE_EXTENT
                    | key_type::FREE_SPACE_BITMAP
            ) {
                out.push(*k);
            }
            Ok(true)
        })?;
        if !out
            .iter()
            .any(|k| k.key_type == key_type::FREE_SPACE_INFO && k.objectid == group.start)
        {
            return Err(Error::UnsupportedFeature(format!(
                "the free-space tree has no information item for the block group at {}",
                group.start
            )));
        }
        Ok(out)
    }

    /// The block group starting at `start`.
    fn block_group_at(&self, start: u64) -> Result<BlockGroup> {
        self.block_groups()?
            .into_iter()
            .find(|g| g.start == start)
            .ok_or(Error::NotFound)
    }

    /// Refuse a group whose extents this does not enumerate, or a volume
    /// where the references do not name every item.
    fn refuse_unenumerable(&self, group: &BlockGroup) -> Result<()> {
        if group.holds_metadata() {
            return Err(Error::UnsupportedFeature(format!(
                "the block group at {} holds tree blocks; only a data block group's extents \
                 are enumerated",
                group.start
            )));
        }
        if !group.holds_data() {
            return Err(Error::UnsupportedFeature(format!(
                "the block group at {} is not a data block group",
                group.start
            )));
        }
        if let Some((subvolume, generation)) = self.snapshotted_subvolume()? {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {subvolume} was snapshotted in generation {generation}: a tree \
                 block the snapshot shares names extents from a tree no reference records, \
                 and following those is not implemented"
            )));
        }
        Ok(())
    }

    /// The first subvolume the root tree records as snapshotted, and the
    /// generation it last was, if any was.
    ///
    /// A deleted snapshot leaves its subvolume's `last_snapshot` behind,
    /// so this also refuses a volume where nothing is shared any more:
    /// the cautious side of a field that cannot tell the two apart.
    fn snapshotted_subvolume(&self) -> Result<Option<(u64, u64)>> {
        let reader = self.pool_reader();
        let mut found = None;
        let mut short = None;
        reader
            .tree()
            .for_each(self.sb.root, &mut |k: &DiskKey, d: &[u8]| {
                if k.key_type != ROOT_ITEM_KEY || !crate::subvol::is_subvolume_id(k.objectid) {
                    return Ok(true);
                }
                let Some(field) = d.get(root_item::LAST_SNAPSHOT..root_item::LAST_SNAPSHOT + 8)
                else {
                    short = Some(k.objectid);
                    return Ok(false);
                };
                let last = le64(field, 0);
                if last != 0 {
                    found = Some((k.objectid, last));
                    return Ok(false);
                }
                Ok(true)
            })?;
        if let Some(id) = short {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id}'s root item is too short to say whether it was snapshotted"
            )));
        }
        Ok(found)
    }

    /// Every data extent of `group`, in address order, each with every
    /// reference to it found and accounted for.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] for anything in the group but data
    /// extents and their keyed references, for a shared reference, and
    /// for references that do not add up to the extent's count or name
    /// file items that are not there.
    fn data_extents_in(&self, group: &BlockGroup) -> Result<Vec<DataExtent>> {
        use crate::extent_write::EXTENT_FLAG_DATA;
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        let mut out: Vec<DataExtent> = Vec::new();
        let mut refused: Option<String> = None;
        {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let from = DiskKey {
                objectid: group.start,
                key_type: 0,
                offset: 0,
            };
            tree.for_each_from(root, &from, &mut |k: &DiskKey, data: &[u8]| {
                if k.objectid >= group.end() {
                    return Ok(false);
                }
                match k.key_type {
                    key_type::BLOCK_GROUP_ITEM => {}
                    key_type::EXTENT_ITEM => {
                        if crate::backref::flags(data).is_none_or(|f| f & EXTENT_FLAG_DATA == 0) {
                            refused = Some(format!(
                                "the extent at {} in the data block group at {} is not \
                                 recorded as data",
                                k.objectid, group.start
                            ));
                            return Ok(false);
                        }
                        out.push(DataExtent {
                            at: k.objectid,
                            len: k.offset,
                            body: data.to_vec(),
                            keyed: Vec::new(),
                            items: Vec::new(),
                        });
                    }
                    EXTENT_DATA_REF => match out.last_mut() {
                        Some(e) if e.at == k.objectid => e.keyed.push(OwnedItem {
                            key: *k,
                            data: data.to_vec(),
                        }),
                        _ => {
                            refused = Some(format!(
                                "the extent tree holds a data reference at {} with no \
                                 extent item before it",
                                k.objectid
                            ));
                            return Ok(false);
                        }
                    },
                    SHARED_DATA_REF => {
                        refused = Some(shared(k.objectid));
                        return Ok(false);
                    }
                    other => {
                        refused = Some(format!(
                            "the data block group at {} holds an item of type {other} at {}, \
                             which is neither a data extent nor a reference to one",
                            group.start, k.objectid
                        ));
                        return Ok(false);
                    }
                }
                Ok(true)
            })?;
        }
        if let Some(why) = refused {
            return Err(Error::UnsupportedFeature(why));
        }
        for extent in &mut out {
            extent.items = self.file_items_naming(extent)?;
        }
        Ok(out)
    }

    /// Every file item naming `extent`, found through its references, and
    /// refused unless they account for every one of them.
    fn file_items_naming(&self, extent: &DataExtent) -> Result<Vec<(u64, OwnedItem)>> {
        // Each keyed data reference, inline or an item of its own: the
        // tree, the inode, the file offset of the extent's first byte, and
        // how many of that inode's items name it there.
        let mut refs: Vec<(u64, u64, u64, u32)> = Vec::new();
        for r in crate::backref::inline_refs(key_type::EXTENT_ITEM, &extent.body)? {
            match r.kind {
                EXTENT_DATA_REF if r.payload.len() >= DATA_REF_SIZE => {
                    refs.push(data_ref(&r.payload));
                }
                SHARED_DATA_REF => return Err(Error::UnsupportedFeature(shared(extent.at))),
                kind => {
                    return Err(Error::UnsupportedFeature(format!(
                        "the data extent at {} carries a reference of type {kind}, which is \
                         not a data reference",
                        extent.at
                    )))
                }
            }
        }
        for item in &extent.keyed {
            if item.data.len() < DATA_REF_SIZE {
                return Err(Error::UnsupportedFeature(format!(
                    "the data reference item of the extent at {} is {} bytes, shorter than \
                     a data reference",
                    extent.at,
                    item.data.len()
                )));
            }
            refs.push(data_ref(&item.data));
        }
        let total: u64 = refs.iter().map(|r| u64::from(r.3)).sum();
        let count = extent.body.get(..8).map(|b| le64(b, 0));
        if count != Some(total) {
            return Err(Error::UnsupportedFeature(format!(
                "the data extent at {} records {} references, and its data references \
                 account for {total}",
                extent.at,
                count.map_or("an unreadable number of".to_string(), |c| c.to_string())
            )));
        }

        let reader = self.pool_reader();
        let tree = reader.tree();
        let mut out: Vec<(u64, OwnedItem)> = Vec::new();
        for (root, ino, offset, want) in refs {
            let at = self.tree_root(root)?;
            // An item naming the extent at reference offset `offset` sits
            // at that file offset or after it, by however far into the
            // extent it starts.
            let from = DiskKey {
                objectid: ino,
                key_type: EXTENT_DATA_KEY,
                offset,
            };
            let mut found = 0u32;
            tree.for_each_from(at, &from, &mut |k: &DiskKey, d: &[u8]| {
                if k.objectid != ino || k.key_type != EXTENT_DATA_KEY {
                    return Ok(false);
                }
                let names_it = d.len() >= fe::REGULAR_SIZE
                    && matches!(d[fe::TYPE], EXTENT_REGULAR | EXTENT_PREALLOC)
                    && le64(d, fe::DISK_BYTENR) == extent.at
                    && k.offset.checked_sub(le64(d, fe::OFFSET)) == Some(offset);
                if names_it {
                    found += 1;
                    out.push((
                        root,
                        OwnedItem {
                            key: *k,
                            data: d.to_vec(),
                        },
                    ));
                }
                Ok(true)
            })?;
            if found != want {
                return Err(Error::UnsupportedFeature(format!(
                    "the data extent at {} is recorded as named {want} time(s) by inode {ino} \
                     of tree {root} at {offset}, and that inode has {found} such item(s)",
                    extent.at
                )));
            }
        }
        Ok(out)
    }
}

/// The refusal of a data extent named by a shared reference.
fn shared(at: u64) -> String {
    format!(
        "the data extent at {at} is named by a shared reference, through the tree block \
         holding the pointer rather than its tree, as snapshots and the kernel's balance \
         leave; following those is not implemented"
    )
}

/// A keyed data reference's tree, inode, offset and count.
fn data_ref(b: &[u8]) -> (u64, u64, u64, u32) {
    let count = u32::from_le_bytes(b[24..28].try_into().expect("4 bytes"));
    (le64(b, 0), le64(b, 8), le64(b, 16), count)
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// One `DEV_EXTENT`: a stripe of a chunk, on a device.
#[derive(Debug, Clone, Copy)]
struct DevExtent {
    devid: u64,
    physical: u64,
    chunk_offset: u64,
    length: u64,
    chunk_tree_uuid: [u8; 16],
}

/// A `CHUNK_ITEM`'s body: `length` logical bytes of `flags`, over
/// `stripes`, each `(devid, physical offset, device UUID)`.
fn chunk_item(
    length: u64,
    flags: u64,
    sectorsize: u32,
    stripes: &[(u64, u64, [u8; 16])],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(48 + 32 * stripes.len());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&objectid::EXTENT_TREE.to_le_bytes()); // owner
    out.extend_from_slice(&DEFAULT_STRIPE_LEN.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&(DEFAULT_STRIPE_LEN as u32).to_le_bytes()); // io_align
    out.extend_from_slice(&(DEFAULT_STRIPE_LEN as u32).to_le_bytes()); // io_width
    out.extend_from_slice(&sectorsize.to_le_bytes());
    out.extend_from_slice(&(stripes.len() as u16).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // sub_stripes
    for (devid, physical, uuid) in stripes {
        out.extend_from_slice(&devid.to_le_bytes());
        out.extend_from_slice(&physical.to_le_bytes());
        out.extend_from_slice(uuid);
    }
    out
}

/// A `DEV_EXTENT`'s body: a stripe of `length` bytes of the chunk at
/// `chunk_offset`, in the chunk tree.
fn dev_extent_item(chunk_offset: u64, length: u64, chunk_tree_uuid: [u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(dev_extent::SIZE);
    out.extend_from_slice(&objectid::CHUNK_TREE.to_le_bytes());
    out.extend_from_slice(&objectid::FIRST_CHUNK_TREE.to_le_bytes());
    out.extend_from_slice(&chunk_offset.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&chunk_tree_uuid);
    debug_assert_eq!(out.len(), dev_extent::SIZE);
    out
}

/// Where `count` stripes of `len` bytes go on a device of `device_bytes`
/// whose extents `taken` already holds, each `(physical, length)`: the
/// first holes that fit, on MiB boundaries, past the first MiB, and
/// clear of every superblock copy -- so no allocation inside the chunk
/// has to step around one, and the free-space tree's one run covering
/// the group is true.
fn place_stripes(
    taken: &[(u64, u64)],
    device_bytes: u64,
    len: u64,
    count: usize,
) -> Option<Vec<u64>> {
    use crate::superblock::SUPER_OFFSETS;
    if len == 0 {
        return None;
    }
    let mut busy: Vec<(u64, u64)> = taken
        .iter()
        .map(|&(at, n)| (at, at.saturating_add(n)))
        .chain(std::iter::once((0, MIB)))
        .chain(SUPER_OFFSETS.iter().map(|&at| {
            let end = (at + crate::super_write::SUPERBLOCK_SIZE as u64).next_multiple_of(MIB);
            (at - at % MIB, end)
        }))
        .collect();
    busy.sort();
    let mut out = Vec::with_capacity(count);
    let mut at = 0u64;
    let mut i = 0usize;
    while out.len() < count {
        at = at.next_multiple_of(MIB);
        let end = at.checked_add(len)?;
        if end > device_bytes {
            return None;
        }
        // The first busy range ending after `at`: if it starts before the
        // stripe would end, the stripe goes after it.
        while i < busy.len() && busy[i].1 <= at {
            i += 1;
        }
        match busy.get(i) {
            Some(&(start, stop)) if start < end => at = stop,
            _ => {
                out.push(at);
                busy.insert(i, (at, end));
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::Chunk;

    #[test]
    fn a_chunk_item_this_writes_parses_as_the_chunk_it_describes() {
        let uuid = [7u8; 16];
        let item = chunk_item(8 * MIB, bg::DATA, 4096, &[(1, 300 * MIB, uuid)]);
        let chunk = Chunk::parse(1 << 30, &item).expect("the item parses");
        assert_eq!(chunk.length, 8 * MIB);
        assert_eq!(chunk.profile().unwrap(), ChunkProfile::Single);
        assert!(chunk.is_data());
        assert_eq!(chunk.encoded_len(), item.len());
        assert_eq!(
            chunk
                .stripes
                .iter()
                .map(|s| (s.devid, s.offset, s.dev_uuid))
                .collect::<Vec<_>>(),
            vec![(1, 300 * MIB, uuid)]
        );
        let at = (1 << 30) + 5000;
        assert_eq!(chunk.map_mirror(at, 0).unwrap().physical, 300 * MIB + 5000);
    }

    #[test]
    fn a_dup_chunk_item_maps_both_copies_on_the_same_device() {
        let uuid = [7u8; 16];
        let stripes = [(1, 300 * MIB, uuid), (1, 308 * MIB, uuid)];
        let item = chunk_item(8 * MIB, bg::DATA | bg::DUP, 4096, &stripes);
        let chunk = Chunk::parse(1 << 30, &item).expect("the DUP item parses");
        assert_eq!(chunk.profile().unwrap(), ChunkProfile::Dup);
        assert_eq!(chunk.length, 8 * MIB);
        assert_eq!(chunk.num_stripes, 2);
        assert_eq!(chunk.encoded_len(), item.len());
        for (mirror, &(devid, physical, uuid)) in stripes.iter().enumerate() {
            let stripe = &chunk.stripes[mirror];
            assert_eq!(
                (stripe.devid, stripe.offset, stripe.dev_uuid),
                (devid, physical, uuid)
            );
            assert_eq!(
                chunk.map_mirror((1 << 30) + 5000, mirror).unwrap().physical,
                physical + 5000
            );
        }
    }

    #[test]
    fn stripes_go_in_the_first_holes_past_the_first_mib() {
        // An extent at 1 MiB of 8 MiB, and one at 20 MiB of 4 MiB.
        let taken = [(MIB, 8 * MIB), (20 * MIB, 4 * MIB)];
        assert_eq!(
            place_stripes(&taken, 256 * MIB, 8 * MIB, 1),
            Some(vec![9 * MIB])
        );
        // Two: the hole before 20 MiB holds one, the next goes after it.
        assert_eq!(
            place_stripes(&taken, 256 * MIB, 8 * MIB, 2),
            Some(vec![9 * MIB, 24 * MIB])
        );
        // Nothing fits past the device.
        assert_eq!(
            place_stripes(&taken, 30 * MIB, 8 * MIB, 1),
            Some(vec![9 * MIB])
        );
        assert_eq!(place_stripes(&taken, 30 * MIB, 8 * MIB, 2), None);
    }

    #[test]
    fn a_stripe_never_covers_a_superblock_copy() {
        // Everything taken up to 60 MiB: an 8 MiB stripe would cross the
        // copy at 64 MiB, so it starts past it.
        let taken = [(MIB, 59 * MIB)];
        let at = place_stripes(&taken, 512 * MIB, 8 * MIB, 1).unwrap()[0];
        assert!(at >= 65 * MIB, "a stripe at {at} covers the copy at 64 MiB");
        // And one that fits before it stays there.
        assert_eq!(
            place_stripes(&taken, 512 * MIB, 4 * MIB, 1),
            Some(vec![60 * MIB])
        );
    }

    #[test]
    fn a_misaligned_extent_end_moves_the_stripe_to_the_next_mib() {
        let taken = [(MIB, 8 * MIB + 4096)];
        assert_eq!(
            place_stripes(&taken, 256 * MIB, MIB, 1),
            Some(vec![10 * MIB])
        );
    }

    /// Tests that need the oracle VM; the unit tier skips them by name.
    mod needs_host {
        use super::*;

        /// A private copy of an oracle-made image, removed even on a panic.
        struct RefusalImage(std::path::PathBuf);

        impl RefusalImage {
            fn new(name: &str, source: &str) -> Self {
                use fs_btrfs_test_support::{fixture, temp_path};
                let dir = std::path::PathBuf::from(temp_path!("metadata-refused-{name}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::copy(fixture(source), dir.join("fs.img")).unwrap();
                Self(dir)
            }

            fn path(&self) -> std::path::PathBuf {
                self.0.join("fs.img")
            }

            fn digest(&self) -> Vec<u8> {
                use sha2::{Digest, Sha256};
                use std::io::Read;
                let mut file = std::fs::File::open(self.path()).unwrap();
                let mut hash = Sha256::new();
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = file.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    hash.update(&buf[..n]);
                }
                hash.finalize().to_vec()
            }

            fn refused(&self, fs: &mut Filesystem, start: u64, reason: &str) {
                let before = self.digest();
                let generation = fs.sb.generation;
                let emptying = fs.emptying;
                for relocate in [false, true] {
                    let result = if relocate {
                        fs.relocate_block_group(start, None).map(|_| ())
                    } else {
                        fs.copy_tree_blocks_out_of(start)
                    };
                    assert_eq!(
                        self.digest(),
                        before,
                        "refusal changed the image: {result:?}"
                    );
                    assert_eq!(
                        fs.sb.generation, generation,
                        "refusal committed a generation"
                    );
                    assert_eq!(
                        fs.emptying, emptying,
                        "refusal changed the allocator exclusion"
                    );
                    assert!(
                        matches!(&result, Err(Error::UnsupportedFeature(why)) if why.contains(reason)),
                        "expected UnsupportedFeature naming {reason}: {result:?}"
                    );
                }
            }
        }

        impl Drop for RefusalImage {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// The allocator's DUP chunk is judged before and after removing
        /// the old empty single data group, so the kernel has to use DUP.
        #[test]
        fn an_empty_dup_chunk_has_two_device_extents_and_the_kernel_writes_and_scrubs_it() {
            use fs_btrfs_test_support::{
                assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok,
                oracle, temp_path,
            };
            use fs_core::{BlockDevice, FileDevice};
            use std::sync::Arc;

            let dir = std::path::PathBuf::from(temp_path!("allocate-dup-chunk"));
            std::fs::create_dir_all(&dir).unwrap();
            let image = RefusalImage(dir);
            let img = image.path();
            std::fs::File::create(&img)
                .unwrap()
                .set_len(256 * MIB)
                .unwrap();
            let made = oracle("mkfs.btrfs")
                .args([
                    "-q", "-f", "-m", "single", "-d", "single", "-s", "4096", "-n", "16384",
                ])
                .arg(&img)
                .output();
            assert!(made.status.success(), "mkfs.btrfs: {made:?}");
            guest_kernel_write_ok(
                &img.to_string_lossy(),
                "write metadata before DUP allocation",
                "printf 'before allocation\\n' > \"$MNT/before\"\nsync",
            );

            let new = {
                let device = Arc::new(FileDevice::open_rw(&img).unwrap());
                let mut fs = Filesystem::mount_rw(device as Arc<dyn BlockDevice>).unwrap();
                let old = fs
                    .block_groups()
                    .unwrap()
                    .into_iter()
                    .find(|g| g.holds_data() && !g.holds_metadata())
                    .unwrap();
                assert_eq!(old.used, 0, "the kernel's small file stays inline");
                let devid = fs.sb.dev_item.devid;
                let before = fs.sb.dev_item.bytes_used;
                let new = fs
                    .allocate_chunk(bg::DATA | bg::DUP, old.length)
                    .expect("allocate an empty DUP data chunk");
                assert_eq!(fs.sb.dev_item.bytes_used, before + 2 * old.length);
                let (_, item) = fs.dev_item(devid).unwrap();
                assert_eq!(le64(&item, DEV_ITEM_BYTES_USED), before + 2 * old.length);
                let group = fs.block_group_at(new).unwrap();
                assert_eq!(
                    (group.flags, group.length, group.used),
                    (bg::DATA | bg::DUP, old.length, 0)
                );
                let chunk = fs.map.chunk_for(new).unwrap();
                assert_eq!(chunk.profile().unwrap(), ChunkProfile::Dup);
                assert_eq!(chunk.num_stripes, 2);
                let extents: Vec<_> = fs
                    .dev_extents()
                    .unwrap()
                    .into_iter()
                    .filter(|e| e.chunk_offset == new)
                    .collect();
                assert_eq!(extents.len(), 2);
                assert_ne!(chunk.stripes[0].offset, chunk.stripes[1].offset);
                for stripe in &chunk.stripes {
                    assert_eq!(stripe.devid, devid);
                    assert!(extents.iter().any(|e| e.devid == devid
                        && e.physical == stripe.offset
                        && e.length == old.length));
                }
                assert_btrfs_check_clean(&img, "after allocating the empty DUP chunk");
                fs.remove_block_group(old.start)
                    .expect("remove the empty single data group");
                assert_eq!(fs.sb.dev_item.bytes_used, before + old.length);
                new
            };
            let chunks = dump_tree(&img, "chunk");
            assert!(chunks.contains(&format!("CHUNK_ITEM {new})")), "{chunks}");
            assert!(chunks.contains("type DATA|DUP"), "{chunks}");
            assert_btrfs_check_clean(&img, "after removing the single data group");
            let wrote = guest_kernel_write_ok(
                &img.to_string_lossy(),
                "write into the allocated DUP chunk and scrub both copies",
                "cd \"$MNT\"\n\
                 test \"$(cat before)\" = 'before allocation'\n\
                 btrfs filesystem df \"$MNT\"\n\
                 head -c 1048576 /dev/urandom > after\n\
                 sync\n\
                 btrfs scrub start -B \"$MNT\"\n\
                 sha256sum after",
            );
            assert!(
                wrote.lines().any(|line| line.starts_with("Data, DUP")),
                "{wrote}"
            );
            assert!(
                !wrote.lines().any(|line| line.starts_with("Data, single")),
                "{wrote}"
            );
            let sum = wrote
                .lines()
                .find(|line| line.ends_with("  after"))
                .expect("the kernel reports the payload's checksum");
            let read = guest_kernel_read_ok(
                &img.to_string_lossy(),
                "read the payload from the allocated DUP chunk",
                "cd \"$MNT\"\nsha256sum after",
            );
            assert!(read.lines().any(|line| line == sum), "{read}");
            assert_btrfs_check_clean(&img, "after the kernel writes and scrubs the DUP chunk");
        }

        #[test]
        fn metadata_evacuation_refuses_an_unreplayed_log_without_writing() {
            use fs_core::{BlockDevice, FileDevice};
            use std::sync::Arc;

            let image = RefusalImage::new("log", "dirtylog/btrfs-dirty-log-inode.img");
            let device = Arc::new(FileDevice::open_rw(image.path()).unwrap());
            // mount_rw replays this log. Open the committed trees without
            // replay, then supply the writer to exercise evacuation itself.
            let mut fs = Filesystem::mount_ignoring_log(device.clone()).unwrap();
            assert_ne!(fs.sb.log_root, 0, "the kernel fixture must retain its log");
            assert!(fs.log().unwrap().is_some(), "the log tree must be readable");
            fs.writable = Some(device as Arc<dyn BlockDevice>);
            let group = fs
                .block_groups()
                .unwrap()
                .into_iter()
                .find(|g| g.holds_metadata() && g.flags & bg::SYSTEM == 0)
                .unwrap();
            image.refused(&mut fs, group.start, "log tree");
        }

        #[test]
        fn metadata_evacuation_refuses_a_snapshot_shared_tree_block_without_writing() {
            use fs_btrfs_test_support::dump_tree;
            use fs_core::{BlockDevice, FileDevice};
            use std::sync::Arc;

            let image = RefusalImage::new("shared", "snapshot/btrfs-nodatacow-snapshot.img");
            // btrfs-progs, rather than our extent reader, identifies a
            // tree block whose extent record counts multiple references.
            let dump = dump_tree(&image.path(), "extent");
            let lines: Vec<_> = dump.lines().collect();
            let shared = lines
                .windows(2)
                .find_map(|pair| {
                    let key = pair[0].split("key (").nth(1)?;
                    let mut words = key.split_whitespace();
                    let address = words.next()?.parse::<u64>().ok()?;
                    if words.next()? != "METADATA_ITEM" {
                        return None;
                    }
                    let refs = pair[1]
                        .split_whitespace()
                        .skip_while(|w| *w != "refs")
                        .nth(1)?
                        .parse::<u64>()
                        .ok()?;
                    (refs > 1).then_some(address)
                })
                .expect("the kernel snapshot must share a tree block");
            let device = Arc::new(FileDevice::open_rw(image.path()).unwrap());
            let mut fs = Filesystem::mount_rw(device as Arc<dyn BlockDevice>).unwrap();
            let group = fs
                .block_groups()
                .unwrap()
                .into_iter()
                .find(|g| g.holds_metadata() && (g.start..g.end()).contains(&shared))
                .unwrap();
            image.refused(&mut fs, group.start, "shared tree block");
        }

        #[test]
        fn metadata_evacuation_refuses_a_mixed_group_without_writing() {
            use fs_btrfs_test_support::{dump_tree, guest_kernel_write_ok};
            use fs_core::{BlockDevice, FileDevice};
            use std::sync::Arc;

            let image = RefusalImage::new("mixed", "btrfs-mixed.img");
            guest_kernel_write_ok(
                &image.path().to_string_lossy(),
                "populate a mixed data and metadata group",
                "head -c 8192 /dev/urandom > \"$MNT/payload\"; sync",
            );
            assert!(
                dump_tree(&image.path(), "extent").contains("DATA|METADATA"),
                "btrfs-progs must confirm the mixed block group"
            );
            let device = Arc::new(FileDevice::open_rw(image.path()).unwrap());
            let mut fs = Filesystem::mount_rw(device as Arc<dyn BlockDevice>).unwrap();
            let group = fs
                .block_groups()
                .unwrap()
                .into_iter()
                .find(|g| g.holds_metadata() && g.holds_data())
                .unwrap();
            image.refused(&mut fs, group.start, "mixed data and metadata");
        }

        /// While a relocation empties a metadata group, a transaction's
        /// copy-on-write puts no tree block back into it (#574).
        ///
        /// A second metadata chunk is allocated beside the one `mkfs.btrfs`
        /// made, the first is excluded, and a file is created: every block the
        /// commit rewrites must land in the second, so the first group's
        /// `used` cannot grow and the new root tree block is outside it.
        /// Without the exclusion first fit picks the first group, which has
        /// room, and the second stays empty.
        #[test]
        fn no_tree_block_is_placed_in_the_group_being_emptied() {
            use fs_btrfs_test_support::{oracle, temp_path};
            use fs_core::{BlockDevice, FileDevice};
            use std::sync::Arc;

            let dir = std::path::PathBuf::from(temp_path!("alloc-out-of-emptied"));
            std::fs::create_dir_all(&dir).unwrap();
            let img = dir.join("fs.img");
            std::fs::File::create(&img)
                .unwrap()
                .set_len(256 * MIB)
                .unwrap();
            let made = oracle("mkfs.btrfs")
                .args(["-q", "-f", "-m", "single", "-d", "single", "-n", "16384"])
                .arg(&img)
                .output();
            assert!(
                made.status.success(),
                "{}",
                String::from_utf8_lossy(&made.stderr)
            );

            let dev = Arc::new(FileDevice::open_rw(&img).unwrap());
            let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap();
            let metadata = |fs: &Filesystem| -> Vec<BlockGroup> {
                fs.block_groups()
                    .unwrap()
                    .into_iter()
                    .filter(|g| g.holds_metadata() && g.flags & bg::SYSTEM == 0)
                    .collect()
            };
            let old = metadata(&fs)[0];
            let new = fs
                .allocate_chunk(old.flags, old.length)
                .expect("a second metadata chunk");
            fs.emptying = Some(old.start);
            fs.create(256, b"after", 0o100644, 0, 0)
                .expect("a file created while the group is excluded");

            let after = metadata(&fs);
            let _ = std::fs::remove_dir_all(&dir);
            let used = |start: u64| after.iter().find(|g| g.start == start).unwrap().used;
            assert!(
                used(old.start) <= old.used,
                "the excluded group's used grew from {} to {}",
                old.used,
                used(old.start)
            );
            assert!(used(new) > 0, "nothing was placed in the new group");
            let root = fs.sb.root;
            assert!(
                !(old.start..old.start + old.length).contains(&root),
                "the new root tree block {root} is inside the excluded group"
            );
        }

        /// Every tree block in a metadata group is copied-on-write out of
        /// it in one committed transaction (#578).
        ///
        /// A second metadata chunk is allocated beside the one `mkfs.btrfs`
        /// made, a few files give the trees something to hold, and the pass
        /// runs on the first group: `btrfs check` must find the volume
        /// clean, and no tree block `dump-tree` prints may lie in the old
        /// group's range.
        #[test]
        fn every_tree_block_is_copied_out_of_a_metadata_group() {
            use fs_btrfs_test_support::{oracle, temp_path};
            use fs_core::{BlockDevice, FileDevice};
            use std::sync::Arc;

            let dir = std::path::PathBuf::from(temp_path!("metadata-cow-out"));
            std::fs::create_dir_all(&dir).unwrap();
            let img = dir.join("fs.img");
            std::fs::File::create(&img)
                .unwrap()
                .set_len(256 * MIB)
                .unwrap();
            let made = oracle("mkfs.btrfs")
                .args(["-q", "-f", "-m", "single", "-d", "single", "-n", "16384"])
                .arg(&img)
                .output();
            assert!(
                made.status.success(),
                "{}",
                String::from_utf8_lossy(&made.stderr)
            );

            let old = {
                let dev = Arc::new(FileDevice::open_rw(&img).unwrap());
                let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap();
                for i in 0..8u8 {
                    fs.create(256, format!("f{i}").as_bytes(), 0o100644, 0, 0)
                        .expect("a file to hold");
                }
                let old = fs
                    .block_groups()
                    .unwrap()
                    .into_iter()
                    .find(|g| g.holds_metadata() && g.flags & bg::SYSTEM == 0)
                    .expect("a metadata group");
                fs.allocate_chunk(old.flags, old.length)
                    .expect("a second metadata chunk");
                fs.copy_tree_blocks_out_of(old.start)
                    .expect("the tree blocks copied out");
                old
            };

            let checked = oracle("btrfs")
                .args(["check", "--readonly"])
                .arg(&img)
                .output();
            let dumped = oracle("btrfs")
                .args(["inspect-internal", "dump-tree"])
                .arg(&img)
                .output();
            let _ = std::fs::remove_dir_all(&dir);
            assert!(
                checked.status.success(),
                "btrfs check: {}{}",
                String::from_utf8_lossy(&checked.stdout),
                String::from_utf8_lossy(&checked.stderr)
            );
            assert!(dumped.status.success());
            let range = old.start..old.start + old.length;
            let inside: Vec<u64> = String::from_utf8_lossy(&dumped.stdout)
                .lines()
                .filter_map(|l| {
                    let mut w = l.split_whitespace();
                    match w.next() {
                        Some("leaf" | "node") => w.next()?.parse().ok(),
                        _ => None,
                    }
                })
                .filter(|b| range.contains(b))
                .collect();
            assert!(
                inside.is_empty(),
                "tree blocks still in the old group: {inside:?}"
            );
        }
    }

    fn group(flags: u64, used: u64) -> BlockGroup {
        BlockGroup {
            start: 1 << 30,
            length: 100 * MIB,
            used,
            flags,
        }
    }

    #[test]
    fn a_balance_selects_by_kind_profile_usage_and_conversion() {
        let data = Balance {
            kinds: bg::DATA,
            ..Default::default()
        };
        assert!(data.selects(&group(bg::DATA, 0)).unwrap());
        assert!(!data.selects(&group(bg::METADATA | bg::DUP, 0)).unwrap());

        let dup_only = Balance {
            profiles: Some(vec![ChunkProfile::Dup]),
            ..data.clone()
        };
        assert!(!dup_only.selects(&group(bg::DATA, 0)).unwrap());
        assert!(dup_only.selects(&group(bg::DATA | bg::DUP, 0)).unwrap());

        // A group already in the profile asked for stays where it is.
        let to_dup = Balance {
            convert: Some(ChunkProfile::Dup),
            ..data.clone()
        };
        assert!(to_dup.selects(&group(bg::DATA, 50 * MIB)).unwrap());
        assert!(!to_dup
            .selects(&group(bg::DATA | bg::DUP, 50 * MIB))
            .unwrap());

        // Less than the share asked for; zero takes only empty groups.
        let half = Balance {
            usage: Some(50),
            ..data.clone()
        };
        assert!(half.selects(&group(bg::DATA, 50 * MIB - 1)).unwrap());
        assert!(!half.selects(&group(bg::DATA, 50 * MIB)).unwrap());
        let empty = Balance {
            usage: Some(0),
            ..data
        };
        assert!(empty.selects(&group(bg::DATA, 0)).unwrap());
        assert!(!empty.selects(&group(bg::DATA, 4096)).unwrap());
    }

    #[test]
    fn every_profile_has_the_bits_its_chunk_type_is_read_back_by() {
        for p in [
            ChunkProfile::Single,
            ChunkProfile::Dup,
            ChunkProfile::Raid0,
            ChunkProfile::Raid1,
            ChunkProfile::Raid1c3,
            ChunkProfile::Raid1c4,
            ChunkProfile::Raid10,
            ChunkProfile::Raid5,
            ChunkProfile::Raid6,
        ] {
            assert_eq!(
                ChunkProfile::from_type(bg::DATA | profile_bits(p)).unwrap(),
                p
            );
        }
    }
}
