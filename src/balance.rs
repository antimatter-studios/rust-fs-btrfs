//! Moving a block group's contents somewhere else: balance, and converting
//! a group to another profile (#304).
//!
//! A block group is relocated in three kinds of committed transaction,
//! each of which leaves a filesystem the kernel and `btrfs check` accept
//! on its own, so a crash between any two is harmless:
//!
//! 1. **A new chunk** is allocated: its `CHUNK_ITEM` in the chunk tree,
//!    a `DEV_EXTENT` in the device tree for each stripe, an empty
//!    `BLOCK_GROUP_ITEM` in the extent tree, a `FREE_SPACE_INFO` and one
//!    free run covering it in the free-space tree, and the device's
//!    `bytes_used` raised in the chunk tree's device item and in the
//!    superblock's copy of it. Its profile is the old group's, or the one
//!    a conversion asks for: the chunk's `type` is what decides how many
//!    copies the bytes written into it get.
//! 2. **The extents move**, a few at a time: each data extent's bytes are
//!    read (verified against their checksums, any copy that verifies) and
//!    written into the new group, every file item naming the extent is
//!    pointed at the new address, its `EXTENT_ITEM` and any back reference
//!    filed as an item of its own move with it unchanged, its digests are
//!    filed under the new sectors, and the free-space tree and both
//!    groups' `used` follow. The extent keeps its length, its references
//!    and its generation; only where it is changes.
//! 3. **The old group goes**, once nothing is left in it: its chunk item,
//!    device extents, block group item and free-space records, and the
//!    device's `bytes_used` comes down again.
//!
//! The kernel relocates the same way, in several transactions, through a
//! relocation tree; this needs none, because a file item is pointed at
//! the new address directly, in a copy-on-write of the leaf holding it.
//!
//! # What it refuses
//!
//! Each refusal names the case, and the ones found before the first
//! transaction leave the filesystem exactly as it was:
//!
//! - a **metadata** or **system** group, or a mixed one: moving tree
//!   blocks out of a group needs the allocator kept out of it, and a
//!   system group is also in the superblock's chunk array;
//! - a volume of **more than one device**, or a profile other than single
//!   or DUP: where the stripes of a new chunk go on a pool is not decided
//!   here yet;
//! - a volume with **no free-space tree**: the free-space cache that
//!   stands in for it names each group by an inode this does not write;
//! - a data extent referred to by a **shared** reference — one naming the
//!   tree block that holds the pointer rather than the tree, which
//!   snapshots and the kernel's own balance leave — or whose references
//!   do not add up to its count;
//! - a file item in a tree block a **snapshot shares**: pointing it at the
//!   new address would change the snapshot too. This one is found while
//!   planning the first move, after the new chunk is committed; the chunk
//!   is then removed again, empty, and the refusal returned.
//!
//! A refusal after an extent has moved leaves both groups in use, each
//! consistent, and a second balance carries on from there.

use std::collections::BTreeMap;

use crate::block_group::BlockGroup;
use crate::chunk::{
    block_group as bg, key_type, objectid, ChunkProfile, DiskKey, DEFAULT_STRIPE_LEN,
};
use crate::commit::DeviceChange;
use crate::error::{Error, Result};
use crate::fs::{file_extent as fe, Filesystem, EXTENT_DATA_KEY, EXTENT_PREALLOC, EXTENT_REGULAR};
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::transaction::{DataWrite, ItemEdit, Relocation};

/// How many rounds a relocation's plan has to close over its own
/// bookkeeping.
const PLAN_ROUNDS: usize = 64;

/// How many data extents one transaction moves. Each adds its records,
/// digests and file items to the leaves the transaction rewrites, and a
/// leaf the change overfills is split once at most.
const EXTENTS_PER_TRANSACTION: usize = 16;

/// How much of an extent is read and written at a time while it is
/// copied: an extent can be 128 MiB, and nothing needs all of it at once.
const COPY_PIECE: u64 = 1 << 20;

/// Where a new chunk's stripes start and how they are aligned: the
/// kernel leaves the first MiB of every device alone, and places device
/// extents on MiB boundaries.
const MIB: u64 = 1 << 20;

/// `BTRFS_DEV_EXTENT_KEY`.
const DEV_EXTENT_KEY: u8 = 204;
/// `BTRFS_EXTENT_DATA_REF_KEY`, keyed or inline.
const EXTENT_DATA_REF: u8 = crate::backref::EXTENT_DATA_REF;

/// `BTRFS_AVAIL_ALLOC_BIT_SINGLE`: how the kernel's balance arguments
/// spell "convert to single", since single has no profile bit of its own.
pub const AVAIL_ALLOC_BIT_SINGLE: u64 = 1 << 48;

/// Byte offsets within `struct btrfs_dev_extent`.
mod dev_extent {
    /// The tree the chunk is in: the chunk tree.
    pub const CHUNK_TREE: usize = 0;
    /// The objectid the chunk item is filed under.
    pub const CHUNK_OBJECTID: usize = 8;
    /// The chunk's logical address.
    pub const CHUNK_OFFSET: usize = 16;
    /// The stripe's length on the device.
    pub const LENGTH: usize = 24;
    /// The chunk tree's UUID.
    pub const CHUNK_TREE_UUID: usize = 32;
    /// The whole item.
    pub const SIZE: usize = 48;
}

/// Where a device item's `bytes_used` is, in the chunk tree's copy.
const DEV_ITEM_BYTES_USED: usize = 16;

/// Which block groups [`Filesystem::balance`] relocates, and what it makes
/// of them: the kernel's balance filters, for the ones it implements.
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
    fn selects(&self, group: &BlockGroup) -> Result<bool> {
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

/// One data extent of a group being emptied, and everything that names
/// it.
#[derive(Debug, Clone)]
struct DataExtent {
    /// Where it is.
    at: u64,
    /// Its length on disk, as its `EXTENT_ITEM` key records it.
    len: u64,
    /// Its `EXTENT_ITEM`'s body, which moves unchanged.
    body: Vec<u8>,
    /// Its back references filed as items of their own, under its
    /// address: they move unchanged too.
    keyed: Vec<OwnedItem>,
    /// Every file item naming it, with the tree each is in.
    items: Vec<(u64, OwnedItem)>,
}

impl Filesystem {
    /// Relocate every block group `balance` selects, each into a newly
    /// allocated chunk, and return how many were.
    ///
    /// Which groups are decided once, before the first moves, so the
    /// chunks this allocates are never relocated in turn.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// and [`Error::UnsupportedFeature`] for every refusal the [module
    /// documentation](crate::balance) lists. The groups selected are each
    /// checked before the first is touched; one refused while it is being
    /// moved stops the balance there, with the groups before it moved.
    pub fn balance(&mut self, balance: &Balance) -> Result<usize> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        let mut selected = Vec::new();
        for group in self.block_groups()? {
            if balance.selects(&group)? {
                selected.push(group);
            }
        }
        for group in &selected {
            self.refuse_unrelocatable(group, self.target_flags(group, balance.convert)?)?;
            self.data_extents_in(group, usize::MAX)?;
        }
        for group in &selected {
            self.relocate_block_group(group.start, balance.convert)?;
        }
        Ok(selected.len())
    }

    /// Relocate the block group at `start` into a newly allocated chunk,
    /// of `profile` when given and of its own profile otherwise, and
    /// return where the new group is.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::balance`].
    pub fn relocate_block_group(
        &mut self,
        start: u64,
        profile: Option<ChunkProfile>,
    ) -> Result<u64> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        let group = self.block_group_at(start)?;
        let flags = self.target_flags(&group, profile)?;
        self.refuse_unrelocatable(&group, flags)?;
        // Every reference is accounted for before anything is written.
        self.data_extents_in(&group, usize::MAX)?;

        let new = self.allocate_chunk(flags, group.length)?;
        let mut moved_any = false;
        loop {
            let batch = self.data_extents_in(&group, EXTENTS_PER_TRANSACTION)?;
            if batch.is_empty() {
                break;
            }
            match self.move_data_extents(&batch, new) {
                Ok(()) => moved_any = true,
                Err(e) if !moved_any => {
                    // Nothing has moved: the new chunk is empty, and taking
                    // it away again leaves the filesystem as it was.
                    self.remove_block_group(new)?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        self.remove_block_group(start)?;
        Ok(new)
    }

    /// The block group starting at `start`.
    fn block_group_at(&self, start: u64) -> Result<BlockGroup> {
        self.block_groups()?
            .into_iter()
            .find(|g| g.start == start)
            .ok_or_else(|| Error::UnsupportedFeature(format!("no block group starts at {start}")))
    }

    /// The `type` of the chunk `group` is relocated into: its own, with
    /// its profile bits replaced by `profile`'s when given.
    fn target_flags(&self, group: &BlockGroup, profile: Option<ChunkProfile>) -> Result<u64> {
        Ok(match profile {
            None => group.flags,
            Some(p) => (group.flags & !bg::PROFILE_MASK) | profile_bits(p),
        })
    }

    /// Refuse a group this does not relocate, or a profile it does not
    /// allocate, before anything is written.
    fn refuse_unrelocatable(&self, group: &BlockGroup, flags: u64) -> Result<()> {
        if group.flags & bg::SYSTEM != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "the block group at {} holds the chunk tree, which the superblock's chunk \
                 array also names, and relocating it is not implemented",
                group.start
            )));
        }
        if group.holds_metadata() {
            return Err(Error::UnsupportedFeature(format!(
                "the block group at {} holds tree blocks, and relocating them is not \
                 implemented",
                group.start
            )));
        }
        if self.sb.num_devices != 1 {
            return Err(Error::UnsupportedFeature(format!(
                "this filesystem spans {} devices, and placing a chunk's stripes on a pool \
                 is not implemented",
                self.sb.num_devices
            )));
        }
        let profile = ChunkProfile::from_type(flags)?;
        if !matches!(profile, ChunkProfile::Single | ChunkProfile::Dup) {
            return Err(Error::UnsupportedFeature(format!(
                "a {} chunk needs several devices; one device holds single and DUP",
                profile.name()
            )));
        }
        if self.tree_root(objectid::FREE_SPACE_TREE).is_err() {
            return Err(Error::UnsupportedFeature(
                "this volume has no free-space tree, and the free-space cache that stands in \
                 for it is not maintained here"
                    .into(),
            ));
        }
        Ok(())
    }

    /// The first `limit` data extents of `group`, in address order, each
    /// with every reference to it found and accounted for.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] for anything in the group but data
    /// extents and their keyed references, for a shared reference, and
    /// for references that do not add up to the extent's count or name
    /// file items that are not there.
    fn data_extents_in(&self, group: &BlockGroup, limit: usize) -> Result<Vec<DataExtent>> {
        use crate::extent_write::EXTENT_FLAG_DATA;
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        let mut out: Vec<DataExtent> = Vec::new();
        let mut refused: Option<Error> = None;
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
                let refuse = |why: String| Error::UnsupportedFeature(why);
                match k.key_type {
                    key_type::BLOCK_GROUP_ITEM => {}
                    key_type::EXTENT_ITEM => {
                        let flags = crate::backref::flags(data);
                        if flags.is_none_or(|f| f & EXTENT_FLAG_DATA == 0) {
                            refused = Some(refuse(format!(
                                "the extent at {} in the data block group at {} is not \
                                 recorded as data",
                                k.objectid, group.start
                            )));
                            return Ok(false);
                        }
                        if out.len() == limit {
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
                            refused = Some(refuse(format!(
                                "the extent tree holds a data reference at {} with no \
                                 extent item before it",
                                k.objectid
                            )));
                            return Ok(false);
                        }
                    },
                    other => {
                        refused = Some(refuse(format!(
                            "the data block group at {} holds an item of type {other} at {}, \
                             and moving it is not implemented: shared references and tree \
                             blocks are not",
                            group.start, k.objectid
                        )));
                        return Ok(false);
                    }
                }
                Ok(true)
            })?;
        }
        if let Some(e) = refused {
            return Err(e);
        }
        for extent in &mut out {
            extent.items = self.file_items_naming(extent)?;
        }
        Ok(out)
    }

    /// Every file item naming `extent`, found through its references, and
    /// refused unless they account for every one of them.
    fn file_items_naming(&self, extent: &DataExtent) -> Result<Vec<(u64, OwnedItem)>> {
        let le64 = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().expect("8"));
        let le32 = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().expect("4"));
        // Each keyed data reference, inline or an item of its own: the
        // tree, the inode, the file offset of the extent's first byte, and
        // how many of that inode's items name it there.
        let mut refs: Vec<(u64, u64, u64, u32)> = Vec::new();
        for r in crate::backref::inline_refs(key_type::EXTENT_ITEM, &extent.body)? {
            if r.kind != EXTENT_DATA_REF || r.payload.len() < 28 {
                return Err(Error::UnsupportedFeature(format!(
                    "the data extent at {} carries a reference of type {}, and only keyed \
                     data references are moved",
                    extent.at, r.kind
                )));
            }
            let p = &r.payload;
            refs.push((le64(p, 0), le64(p, 8), le64(p, 16), le32(p, 24)));
        }
        for item in &extent.keyed {
            let d = &item.data;
            if d.len() < 28 {
                return Err(Error::UnsupportedFeature(format!(
                    "the data reference item of the extent at {} is {} bytes, shorter than \
                     a data reference",
                    extent.at,
                    d.len()
                )));
            }
            refs.push((le64(d, 0), le64(d, 8), le64(d, 16), le32(d, 24)));
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

    /// Move `batch`, data extents of one group, into the group at `to`, in
    /// one committed transaction.
    fn move_data_extents(&mut self, batch: &[DataExtent], to: u64) -> Result<()> {
        let target = self.block_group_at(to)?;
        let lens: Vec<u64> = batch.iter().map(|e| e.len).collect();
        let news = self.find_extents_in(std::slice::from_ref(&target), &lens)?;
        let relocations: Vec<Relocation> = batch
            .iter()
            .zip(&news)
            .map(|(e, &new)| Relocation {
                old: e.at,
                new,
                len: e.len,
            })
            .collect();

        // Deletes first, so a leaf is at its smallest before anything is
        // added to it.
        let extent_tree = objectid::EXTENT_TREE;
        let mut deletes: Vec<(u64, ItemEdit)> = Vec::new();
        let mut puts: Vec<(u64, ItemEdit)> = Vec::new();
        for (e, r) in batch.iter().zip(&relocations) {
            deletes.push((
                extent_tree,
                ItemEdit::Delete(DiskKey {
                    objectid: e.at,
                    key_type: key_type::EXTENT_ITEM,
                    offset: e.len,
                }),
            ));
            puts.push((
                extent_tree,
                ItemEdit::Put(OwnedItem {
                    key: DiskKey {
                        objectid: r.new,
                        key_type: key_type::EXTENT_ITEM,
                        offset: e.len,
                    },
                    data: e.body.clone(),
                }),
            ));
            // A keyed reference's offset is a hash of what it names, not
            // of where the extent is, so it moves as it is.
            for item in &e.keyed {
                deletes.push((extent_tree, ItemEdit::Delete(item.key)));
                puts.push((
                    extent_tree,
                    ItemEdit::Put(OwnedItem {
                        key: DiskKey {
                            objectid: r.new,
                            ..item.key
                        },
                        data: item.data.clone(),
                    }),
                ));
            }
            for (tree, item) in &e.items {
                let mut data = item.data.clone();
                data[fe::DISK_BYTENR..fe::DISK_BYTENR + 8].copy_from_slice(&r.new.to_le_bytes());
                puts.push((
                    *tree,
                    ItemEdit::Put(OwnedItem {
                        key: item.key,
                        data,
                    }),
                ));
            }
        }
        let moved: Vec<(u64, u64, u64)> =
            relocations.iter().map(|r| (r.old, r.new, r.len)).collect();
        let csum = crate::csum::CSUM_TREE_OBJECTID;
        let mut tree_edits = deletes;
        tree_edits.extend(puts);
        tree_edits.extend(self.csum_move_edits(&moved)?.into_iter().map(|e| (csum, e)));

        let write = DataWrite {
            tree_edits,
            relocations,
            ..Default::default()
        };
        self.commit_change(&write, DeviceChange::default(), |fs| {
            for r in &write.relocations {
                fs.copy_extent(r)?;
            }
            Ok(())
        })
    }

    /// Copy a relocated extent's bytes to every copy of its new address,
    /// a piece at a time, each piece read as the file read path reads it:
    /// verified against its digests, from whichever copy verifies.
    fn copy_extent(&self, r: &Relocation) -> Result<()> {
        let mut done = 0u64;
        while done < r.len {
            let n = (r.len - done).min(COPY_PIECE);
            let mut buf = vec![0u8; n as usize];
            self.read_data_verified(r.old + done, &mut buf, true)?;
            self.write_logical_all_mirrors(r.new + done, &buf)?;
            done += n;
        }
        Ok(())
    }

    /// Allocate a chunk of `length` logical bytes and `flags`, with its
    /// block group, in one committed transaction, and return its logical
    /// address.
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
        let (dev_item_key, dev_item) = self.dev_item(devid)?;
        let total = le64_at(&dev_item, 8);
        let used = le64_at(&dev_item, DEV_ITEM_BYTES_USED);
        let stripes = place_stripes(&taken, total, length, copies).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "device {devid} has no room for {copies} stripe(s) of {length} bytes"
            ))
        })?;
        let chunk_tree_uuid = extents
            .first()
            .map(|e| e.chunk_tree_uuid)
            .ok_or_else(|| Error::UnsupportedFeature("the device tree has no extents".into()))?;
        let used = used + length * copies as u64;

        let mut dev_item = dev_item;
        dev_item[DEV_ITEM_BYTES_USED..DEV_ITEM_BYTES_USED + 8].copy_from_slice(&used.to_le_bytes());
        let dev_uuid = self.sb.dev_item.uuid;
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
                    data: chunk_item(
                        length,
                        flags,
                        self.sb.sectorsize,
                        &stripes
                            .iter()
                            .map(|&p| (devid, p, dev_uuid))
                            .collect::<Vec<_>>(),
                    ),
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
        let mut group = Vec::with_capacity(crate::block_group::block_group_item::SIZE);
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

        let write = DataWrite {
            tree_edits,
            ..Default::default()
        };
        let device = DeviceChange {
            bytes_used: BTreeMap::from([(devid, used)]),
            ..Default::default()
        };
        self.commit_change(&write, device, |_| Ok(()))?;
        Ok(logical)
    }

    /// Remove the empty block group at `start` and its chunk, in one
    /// committed transaction.
    fn remove_block_group(&mut self, start: u64) -> Result<()> {
        let group = self.block_group_at(start)?;
        if group.used != 0 || !self.data_extents_in(&group, 1)?.is_empty() {
            return Err(Error::UnsupportedFeature(format!(
                "the block group at {start} still holds {} bytes",
                group.used
            )));
        }
        if group.flags & bg::SYSTEM != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "the block group at {start} holds the chunk tree, and removing a chunk the \
                 superblock's chunk array names is not implemented"
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

        let mut tree_edits = vec![(
            objectid::CHUNK_TREE,
            ItemEdit::Delete(DiskKey {
                objectid: objectid::FIRST_CHUNK_TREE,
                key_type: key_type::CHUNK_ITEM,
                offset: start,
            }),
        )];
        // Its stripes, and what each device gets back.
        let stripes: Vec<DevExtent> = self
            .dev_extents()?
            .into_iter()
            .filter(|e| e.chunk_offset == start)
            .collect();
        if stripes.len() != usize::from(chunk.num_stripes) {
            return Err(Error::UnsupportedFeature(format!(
                "the chunk at {start} has {} stripes, and the device tree {} extents for it",
                chunk.num_stripes,
                stripes.len()
            )));
        }
        let mut freed: BTreeMap<u64, u64> = BTreeMap::new();
        for s in &stripes {
            *freed.entry(s.devid).or_default() += s.length;
            tree_edits.push((
                objectid::DEV_TREE,
                ItemEdit::Delete(DiskKey {
                    objectid: s.devid,
                    key_type: DEV_EXTENT_KEY,
                    offset: s.physical,
                }),
            ));
        }
        let mut device = DeviceChange::default();
        for (&devid, &bytes) in &freed {
            let (key, mut item) = self.dev_item(devid)?;
            let used = le64_at(&item, DEV_ITEM_BYTES_USED)
                .checked_sub(bytes)
                .ok_or_else(|| {
                    Error::UnsupportedFeature(format!(
                        "device {devid} records fewer bytes used than the chunk at {start} \
                         takes on it"
                    ))
                })?;
            item[DEV_ITEM_BYTES_USED..DEV_ITEM_BYTES_USED + 8].copy_from_slice(&used.to_le_bytes());
            tree_edits.push((
                objectid::CHUNK_TREE,
                ItemEdit::Put(OwnedItem { key, data: item }),
            ));
            device.bytes_used.insert(devid, used);
        }
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

        let write = DataWrite {
            tree_edits,
            ..Default::default()
        };
        self.commit_change(&write, device, |_| Ok(()))
    }

    /// Plan, render and commit `write`, then remount. `before_commit` runs
    /// once everything is planned and built and before anything is
    /// committed: a relocation copies its extents there.
    fn commit_change(
        &mut self,
        write: &DataWrite,
        device: DeviceChange,
        before_commit: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<()> {
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let plan = self.plan_transaction_closed_with(&[], write, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, write, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the relocation's plan does not move the root tree".into())
        })?;
        let chunk_root = self.planned_chunk_root(&plan);
        let bytes_used = match plan.usage_delta(u64::from(self.sb.nodesize)) {
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
        before_commit(self)?;
        self.commit_device(
            &blocks,
            &Commit {
                generation,
                root,
                bytes_used,
                chunk_root,
                chunk_root_generation: chunk_root.map(|_| generation),
                invalidate_free_space_tree: false,
                ..Default::default()
            },
            device,
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
                chunk_offset: le64_at(d, dev_extent::CHUNK_OFFSET),
                length: le64_at(d, dev_extent::LENGTH),
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

fn le64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
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
/// `chunk_offset`.
fn dev_extent_item(chunk_offset: u64, length: u64, chunk_tree_uuid: [u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(dev_extent::SIZE);
    out.extend_from_slice(&objectid::CHUNK_TREE.to_le_bytes());
    out.extend_from_slice(&objectid::FIRST_CHUNK_TREE.to_le_bytes());
    out.extend_from_slice(&chunk_offset.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&chunk_tree_uuid);
    debug_assert_eq!(out.len(), dev_extent::SIZE);
    debug_assert_eq!(dev_extent::CHUNK_TREE, 0);
    debug_assert_eq!(dev_extent::CHUNK_OBJECTID, 8);
    out
}

/// Where `count` stripes of `len` bytes go on a device of `device_bytes`
/// whose extents `taken` already holds, each `(physical, length)`: the
/// first holes that fit, on MiB boundaries, past the first MiB, and
/// clear of every superblock copy -- so no allocation inside the chunk
/// has to step around one.
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
        let item = chunk_item(
            8 * MIB,
            bg::DATA | bg::DUP,
            4096,
            &[(1, 300 * MIB, uuid), (1, 308 * MIB, uuid)],
        );
        let chunk = Chunk::parse(1 << 30, &item).expect("the item parses");
        assert_eq!(chunk.length, 8 * MIB);
        assert_eq!(chunk.profile().unwrap(), ChunkProfile::Dup);
        assert!(chunk.is_data());
        assert_eq!(chunk.encoded_len(), item.len());
        assert_eq!(
            chunk
                .stripes
                .iter()
                .map(|s| (s.devid, s.offset, s.dev_uuid))
                .collect::<Vec<_>>(),
            vec![(1, 300 * MIB, uuid), (1, 308 * MIB, uuid)]
        );
        // And both copies of a byte are where the stripes say.
        let at = (1 << 30) + 5000;
        assert_eq!(chunk.map_mirror(at, 0).unwrap().physical, 300 * MIB + 5000);
        assert_eq!(chunk.map_mirror(at, 1).unwrap().physical, 308 * MIB + 5000);
    }

    #[test]
    fn stripes_go_in_the_first_holes_past_the_first_mib() {
        // An extent at 1 MiB of 8 MiB, and one at 20 MiB of 4 MiB.
        let taken = [(MIB, 8 * MIB), (20 * MIB, 4 * MIB)];
        assert_eq!(
            place_stripes(&taken, 256 * MIB, 8 * MIB, 1),
            Some(vec![9 * MIB])
        );
        // Two copies: the hole before 20 MiB holds one, the next after it.
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
