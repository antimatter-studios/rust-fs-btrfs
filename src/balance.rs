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

use crate::block_group::BlockGroup;
use crate::chunk::{key_type, objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::{
    file_extent as fe, root_item, Filesystem, EXTENT_DATA_KEY, EXTENT_PREALLOC, EXTENT_REGULAR,
    ROOT_ITEM_KEY,
};
use crate::leaf_edit::OwnedItem;

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

    /// Relocate the block group at `start` into a newly allocated chunk,
    /// of `profile` when given and of its own profile otherwise, and
    /// return where the new group is (#397).
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`]: not implemented yet.
    pub fn relocate_block_group(
        &mut self,
        start: u64,
        profile: Option<crate::chunk::ChunkProfile>,
    ) -> Result<u64> {
        let _ = (start, profile);
        Err(Error::UnsupportedFeature(
            "relocating a block group is not implemented".into(),
        ))
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
