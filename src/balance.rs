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
//! # What it refuses
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
//! [`Filesystem::relocate_block_group`] moves a data group (#397, #493)
//! in committed transactions, each leaving a filesystem the kernel and
//! `btrfs check` accept on its own:
//!
//! 1. **A new chunk** of the group's profile: its `CHUNK_ITEM` in the
//!    chunk tree, a `DEV_EXTENT` in the device tree for its stripe, an
//!    empty `BLOCK_GROUP_ITEM`, a `FREE_SPACE_INFO` and one free run
//!    covering it in the free-space tree, and the device's `bytes_used`
//!    raised in the chunk tree's device item and the superblock's copy.
//! 2. **The extents move**, when the group holds any (#493), at most 64
//!    per committed transaction (#494): each one's bytes are read verified
//!    against their digests and written into the new chunk, then the
//!    batch's commit re-points the file item naming it, moves
//!    its `EXTENT_ITEM` and keyed back references unchanged, files its
//!    checksums under the new sectors, and carries the free-space tree and
//!    both groups' `used` with it.
//! 3. **The old group goes**: its chunk item, device extents, block group
//!    item and free-space records, and `bytes_used` lowered again.
//!
//! Besides everything above, it refuses a system group, a conversion to
//! another profile, any profile but single, a volume of more than one
//! device, one with no free-space tree or with a block group tree, and a
//! group holding an extent more than one file item names, or one with a
//! sector that has no checksum, each before anything is written. A batch
//! that fails after an earlier one committed leaves both groups, every
//! extent in exactly one of them.

use crate::block_group::BlockGroup;
use crate::chunk::{
    block_group as bg, key_type, objectid, ChunkProfile, DiskKey, DEFAULT_STRIPE_LEN,
};
use crate::error::{Error, Result};
use crate::fs::{
    file_extent as fe, root_item, Filesystem, EXTENT_DATA_KEY, EXTENT_PREALLOC, EXTENT_REGULAR,
    ROOT_ITEM_KEY,
};
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

    /// Relocate the data block group at `start` into a newly allocated
    /// chunk of its own profile, and return where the new group is (#397,
    /// #493).
    ///
    /// Committed transactions, each leaving a filesystem the kernel
    /// accepts on its own: the new chunk is allocated, the group's extents
    /// are moved into it a batch per transaction (#494), then the old
    /// group is removed. A crash between them leaves both groups, every
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
        let new = self.allocate_chunk(group.flags, group.length)?;
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

    /// Move `extents`, one batch of a group's data extents, into the group
    /// at `to` in one committed transaction (#493, #494).
    ///
    /// Each extent's bytes are read as the file read path reads them,
    /// verified against their digests from whichever copy verifies, and
    /// written to the new address before anything is committed: the new
    /// chunk is free space until then, so a crash before the superblock
    /// write leaves the files reading their old extents. The commit then
    /// re-points the one file item naming each extent, moves its
    /// `EXTENT_ITEM` and keyed back references unchanged, and files its
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
                "the block group at {} holds the chunk tree, which the superblock's chunk \
                 array also names, and relocating it is not implemented",
                group.start
            )));
        }
        self.refuse_unenumerable(group)?;
        let own = ChunkProfile::from_type(group.flags)?;
        if let Some(p) = profile.filter(|&p| p != own) {
            return Err(Error::UnsupportedFeature(format!(
                "converting a {} block group to {} is not implemented",
                own.name(),
                p.name()
            )));
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
        let extents = self.data_extents_in(group)?;
        let sector = u64::from(self.sb.sectorsize).max(1);
        for e in &extents {
            if e.items.len() != 1 {
                return Err(Error::UnsupportedFeature(format!(
                    "the extent at {} is named by {} file items, and moving an extent more \
                     than one names is not implemented",
                    e.at,
                    e.items.len()
                )));
            }
            if self.data_digests(e.at, e.len)?.len() as u64 != e.len / sector {
                return Err(Error::UnsupportedFeature(format!(
                    "the extent at {} has sectors with no checksum, and moving data stored \
                     without checksums is not implemented",
                    e.at
                )));
            }
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

    /// Allocate a single chunk of `length` logical bytes and `flags`, with
    /// its empty block group, in one committed transaction, and return its
    /// logical address.
    fn allocate_chunk(&mut self, flags: u64, length: u64) -> Result<u64> {
        let devid = self.sb.dev_item.devid;
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
        let stripes = place_stripes(&taken, total, length, 1).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "device {devid} has no room for a stripe of {length} bytes"
            ))
        })?;
        let chunk_tree_uuid = extents
            .first()
            .map(|e| e.chunk_tree_uuid)
            .ok_or_else(|| Error::UnsupportedFeature("the device tree has no extents".into()))?;
        let used = le64(&dev_item, DEV_ITEM_BYTES_USED)
            .checked_add(length)
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
}
