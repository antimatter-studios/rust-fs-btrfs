//! Reading a log tree the kernel has not replayed yet (#266, first slice).
//!
//! An `fsync` does not commit a transaction. It writes what the synced
//! inodes now hold into a **log tree** per subvolume, names those trees
//! from a **log root tree**, and points the superblock's `log_root` at
//! that. The next mount replays the log into the real trees. Until then
//! the committed trees hold the state before the `fsync`, which is why
//! read-only mounts refuse such a volume with [`Error::DirtyLog`]:
//! reading past the log returns bytes an application was told had been
//! durably replaced.
//!
//! # What this reads
//!
//! The log root tree holds one `ROOT_ITEM` per subvolume that has a log,
//! keyed `(TREE_LOG, ROOT_ITEM, subvolume)`, naming that log tree's root.
//! A log tree holds the same kinds of items as the subvolume it logs —
//! `INODE_ITEM`, `INODE_REF`, `DIR_ITEM`/`DIR_INDEX`, `EXTENT_DATA`, and
//! `DIR_LOG_ITEM`/`DIR_LOG_INDEX` ranges saying which part of a
//! directory the log speaks for — plus `EXTENT_CSUM` items for logged
//! data. [`Filesystem::log`] returns every item of every log tree, raw,
//! with accessors for the logged inodes, names and file contents.
//!
//! # Limited replay
//!
//! A writable mount can recover logged inode metadata for existing empty
//! regular files in tree 5, a same-length rewrite of an inline extent,
//! and a regular extent that replaces an existing file's extent over the
//! same range (#375). References must be unchanged; other records, new
//! inodes and namespace changes remain refused (#266).
//! [`Filesystem::mount_ignoring_log`] opens such a volume **read-only**,
//! reading the committed trees as the kernel's `-o ro,nologreplay`
//! does: an explicit opt-in, because what was `fsync`ed is invisible
//! through it except by way of [`Filesystem::log`].

use std::sync::Arc;

use fs_core::BlockRead;

use crate::chunk::DiskKey;
use crate::dir::{self, DirEntry, DIR_INDEX_KEY};
use crate::error::{Error, Result};
use crate::fs::{root_item, Filesystem, EXTENT_DATA_KEY, ROOT_ITEM_KEY};
use crate::inode::{Inode, INODE_ITEM_KEY, INODE_REF_KEY};

/// `BTRFS_TREE_LOG_OBJECTID`: the objectid a log tree's `ROOT_ITEM` is
/// filed under in the log root tree.
pub const TREE_LOG_OBJECTID: u64 = u64::MAX - 5;

/// `BTRFS_DIR_LOG_ITEM_KEY` and `BTRFS_DIR_LOG_INDEX_KEY`: a range of a
/// directory's name hashes, or indexes, the log speaks for.
pub const DIR_LOG_ITEM_KEY: u8 = 60;
pub const DIR_LOG_INDEX_KEY: u8 = 72;

/// One item of a log tree, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogItem {
    pub key: DiskKey,
    pub data: Vec<u8>,
}

/// One subvolume's log tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedTree {
    /// The subvolume it logs: 5 for the top level.
    pub subvolume: u64,
    /// Its root block.
    pub root: u64,
    /// Every item, in key order.
    pub items: Vec<LogItem>,
}

/// Every log tree a volume holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogContents {
    /// The log root tree's root block, from the superblock.
    pub log_root: u64,
    /// One per subvolume with a log, in subvolume order.
    pub trees: Vec<LoggedTree>,
}

impl LoggedTree {
    /// The inodes the log holds an `INODE_ITEM` for, in order.
    pub fn inodes(&self) -> Vec<u64> {
        self.items
            .iter()
            .filter(|i| i.key.key_type == INODE_ITEM_KEY && i.key.offset == 0)
            .map(|i| i.key.objectid)
            .collect()
    }

    /// The logged `INODE_ITEM` of `ino`, parsed.
    pub fn inode(&self, ino: u64) -> Result<Option<Inode>> {
        self.items
            .iter()
            .find(|i| i.key.objectid == ino && i.key.key_type == INODE_ITEM_KEY)
            .map(|i| Inode::parse(&i.data, ino))
            .transpose()
    }

    /// The names the log holds for directory `dir`, from its `DIR_INDEX`
    /// items, in index order.
    pub fn names(&self, dir: u64) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for item in &self.items {
            if item.key.objectid == dir && item.key.key_type == DIR_INDEX_KEY {
                out.extend(dir::parse_dir_items(&item.data)?);
            }
        }
        Ok(out)
    }

    /// The inode the log names `name` in directory `parent`, from its
    /// `INODE_REF` items: what a logged new name, or a logged inode's
    /// name, resolves to.
    pub fn inode_named(&self, parent: u64, name: &[u8]) -> Option<u64> {
        self.items
            .iter()
            .filter(|i| i.key.key_type == INODE_REF_KEY && i.key.offset == parent)
            .find(|i| ref_names(&i.data).any(|n| n == name))
            .map(|i| i.key.objectid)
    }

    /// The logged `EXTENT_DATA` items of `ino`.
    fn extents(&self, ino: u64) -> impl Iterator<Item = &LogItem> {
        self.items
            .iter()
            .filter(move |i| i.key.objectid == ino && i.key.key_type == EXTENT_DATA_KEY)
    }
}

/// Offsets within a file extent item.
mod fe {
    pub const RAM_BYTES: usize = 8;
    pub const COMPRESSION: usize = 16;
    pub const ENCRYPTION: usize = 17;
    pub const OTHER_ENCODING: usize = 18;
    pub const TYPE: usize = 20;
    pub const INLINE_DATA: usize = 21;
    pub const DISK_BYTENR: usize = 21;
    pub const DISK_NUM_BYTES: usize = 29;
    pub const OFFSET: usize = 37;
    pub const NUM_BYTES: usize = 45;
    pub const REGULAR_SIZE: usize = 53;
}

/// The names in an `INODE_REF` item: records of an 8-byte index, a
/// 2-byte length and the name. Stops at a record that runs past the end.
fn ref_names(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let len = usize::from(u16::from_le_bytes(
            data.get(pos + 8..pos + 10)?.try_into().ok()?,
        ));
        let name = data.get(pos + 10..pos + 10 + len)?;
        pos += 10 + len;
        Some(name)
    })
}

/// The records of an `INODE_REF` item: each name's index in the parent,
/// and the name. `None` when a record runs past the end.
fn ref_records(data: &[u8]) -> Option<Vec<(u64, &[u8])>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let index = u64::from_le_bytes(data.get(pos..pos + 8)?.try_into().ok()?);
        let len = usize::from(u16::from_le_bytes(
            data.get(pos + 8..pos + 10)?.try_into().ok()?,
        ));
        out.push((index, data.get(pos + 10..pos + 10 + len)?));
        pos += 10 + len;
    }
    Some(out)
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// Whether a logged file extent item only rewrites the bytes of a
/// committed inline extent at the same length: both inline, neither
/// compressed, encrypted or otherwise encoded, and each one's
/// `ram_bytes` naming exactly the data it carries.
fn same_length_inline(committed: &[u8], logged: &[u8]) -> bool {
    plain_inline(committed) && plain_inline(logged) && committed.len() == logged.len()
}

/// Whether a file extent item is inline and neither compressed, encrypted
/// nor otherwise encoded, its `ram_bytes` naming exactly the data it
/// carries.
fn plain_inline(d: &[u8]) -> bool {
    d.len() > fe::INLINE_DATA
        && d[fe::TYPE] == 0
        && d[fe::COMPRESSION] == 0
        && d[fe::ENCRYPTION] == 0
        && d[fe::OTHER_ENCODING..fe::TYPE] == [0, 0]
        && le64(d, fe::RAM_BYTES) == (d.len() - fe::INLINE_DATA) as u64
}

/// A file extent item that references the whole of one uncompressed,
/// unencrypted regular extent from its first byte: its address and
/// length. `None` for anything else -- inline, prealloc, a hole, a
/// compressed extent, or a window into part of one.
fn whole_regular_extent(d: &[u8]) -> Option<(u64, u64)> {
    if d.len() != fe::REGULAR_SIZE
        || d[fe::TYPE] != 1
        || d[fe::COMPRESSION] != 0
        || d[fe::ENCRYPTION] != 0
        || d[fe::OTHER_ENCODING..fe::TYPE] != [0, 0]
    {
        return None;
    }
    let (bytenr, disk_len) = (le64(d, fe::DISK_BYTENR), le64(d, fe::DISK_NUM_BYTES));
    let whole = le64(d, fe::OFFSET) == 0
        && le64(d, fe::NUM_BYTES) == disk_len
        && le64(d, fe::RAM_BYTES) == disk_len;
    (bytenr != 0 && disk_len != 0 && whole).then_some((bytenr, disk_len))
}

impl Filesystem {
    /// Recover existing empty regular files in tree 5, existing regular
    /// files whose only logged data is a same-length rewrite of their one
    /// uncompressed inline extent, and existing regular files whose logged
    /// data is a regular extent replacing a committed one over the same
    /// range (#375), and regular files the log creates with inline data or
    /// none, named in directories the log does not change (#376).
    /// Everything is validated before the first write; other data, names
    /// and other trees remain refused until their replay has independent
    /// oracle proof.
    pub(crate) fn replay_inode_only_log(&mut self) -> Result<()> {
        use crate::backref::{self, EXTENT_DATA_REF, SHARED_DATA_REF};
        use crate::chunk::{key_type::EXTENT_ITEM, objectid};
        use crate::csum::{EXTENT_CSUM_KEY, EXTENT_CSUM_OBJECTID};
        use crate::extent_write::EXTENT_FLAG_DATA;
        use crate::inode::{INODE_ITEM_SIZE, INODE_NODATASUM};
        use crate::leaf_edit::OwnedItem;
        use crate::transaction::{DataWrite, ItemEdit, RecordEdit};
        use std::collections::{BTreeMap, BTreeSet};

        let log = self.log()?.ok_or(Error::DirtyLog)?;
        if log.trees.len() != 1 || log.trees[0].subvolume != objectid::FS_TREE {
            return Err(Error::DirtyLog);
        }
        self.require_writable_top_level()?;
        let top = &log.trees[0];
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the log replay generation overflows".into())
        })?;
        let last_snapshot = self.fs_tree_last_snapshot()?;
        let mut edits = Vec::new();
        let mut pins = Vec::new();
        // Each extent replacement (#375): the extent records it changes,
        // the extents it gives up whole, and each new extent with whether
        // its file is checksummed.
        let mut records: Vec<RecordEdit> = Vec::new();
        let mut released: Vec<(u64, u64)> = Vec::new();
        let mut news: Vec<(u64, u64, bool)> = Vec::new();
        let mut logged_csums: Vec<&LogItem> = Vec::new();
        {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let mut roots = 0;
            tree.for_each(log.log_root, &mut |key, _| {
                if key.objectid != TREE_LOG_OBJECTID
                    || key.key_type != ROOT_ITEM_KEY
                    || key.offset != objectid::FS_TREE
                {
                    return Err(Error::DirtyLog);
                }
                roots += 1;
                Ok(true)
            })?;
            if roots != 1 {
                return Err(Error::DirtyLog);
            }
            // Inodes the log creates (#376): a logged inode item the
            // subvolume does not hold. Each is inserted as logged, and each
            // of its names is entered in its parent from its reference,
            // which is all the log says of the name: an fsync of a new file
            // logs no directory item for it.
            let mut created: BTreeSet<u64> = BTreeSet::new();
            for ino in top.inodes() {
                let at = DiskKey {
                    objectid: ino,
                    key_type: INODE_ITEM_KEY,
                    offset: 0,
                };
                if tree.search(self.fs_tree_root, &at)?.is_none() {
                    created.insert(ino);
                }
            }
            let mut grown: BTreeMap<u64, u64> = BTreeMap::new();
            for &ino in &created {
                let inode = top.inode(ino)?.ok_or(Error::DirtyLog)?;
                if !inode.is_regular_file() || ino < crate::inode::FIRST_FREE_OBJECTID {
                    return Err(Error::DirtyLog);
                }
                for kind in [INODE_REF_KEY, EXTENT_DATA_KEY, dir::XATTR_ITEM_KEY] {
                    if !tree.find_all(self.fs_tree_root, ino, kind)?.is_empty() {
                        return Err(Error::DirtyLog);
                    }
                }
                let mut names = 0u64;
                let mut nbytes = 0u64;
                let mut raw = None;
                for item in top.items.iter().filter(|i| i.key.objectid == ino) {
                    match item.key.key_type {
                        INODE_ITEM_KEY if item.key.offset == 0 => raw = Some(item.data.clone()),
                        INODE_REF_KEY => {
                            let parent = item.key.offset;
                            let at = DiskKey {
                                objectid: parent,
                                key_type: INODE_ITEM_KEY,
                                offset: 0,
                            };
                            let dir_item = tree
                                .search(self.fs_tree_root, &at)?
                                .ok_or(Error::DirtyLog)?;
                            // A directory the log also speaks for is #377's.
                            if !Inode::parse(&dir_item.data, parent)?.is_dir()
                                || top.inode(parent)?.is_some()
                            {
                                return Err(Error::DirtyLog);
                            }
                            for (index, name) in ref_records(&item.data).ok_or(Error::DirtyLog)? {
                                let by_hash = DiskKey {
                                    objectid: parent,
                                    key_type: dir::DIR_ITEM_KEY,
                                    offset: dir::name_hash(name),
                                };
                                let by_index = DiskKey {
                                    objectid: parent,
                                    key_type: DIR_INDEX_KEY,
                                    offset: index,
                                };
                                if tree.search(self.fs_tree_root, &by_hash)?.is_some()
                                    || tree.search(self.fs_tree_root, &by_index)?.is_some()
                                {
                                    return Err(Error::DirtyLog);
                                }
                                let entry = crate::namespace::dir_entry(
                                    ino,
                                    generation,
                                    dir::ftype::REG_FILE,
                                    name,
                                );
                                edits.push(ItemEdit::Put(OwnedItem {
                                    key: by_hash,
                                    data: entry.clone(),
                                }));
                                edits.push(ItemEdit::Put(OwnedItem {
                                    key: by_index,
                                    data: entry,
                                }));
                                *grown.entry(parent).or_default() += 2 * name.len() as u64;
                                names += 1;
                            }
                            edits.push(ItemEdit::Put(OwnedItem {
                                key: item.key,
                                data: item.data.clone(),
                            }));
                        }
                        // Only inline data, whole and plain, from offset 0.
                        EXTENT_DATA_KEY if item.key.offset == 0 && plain_inline(&item.data) => {
                            nbytes += le64(&item.data, fe::RAM_BYTES);
                            edits.push(ItemEdit::Put(OwnedItem {
                                key: item.key,
                                data: item.data.clone(),
                            }));
                        }
                        _ => return Err(Error::DirtyLog),
                    }
                }
                // The kernel logs a new inode's `nbytes` as 0 and counts
                // the data replay adds; here, its inline extent.
                let mut raw = raw.ok_or(Error::DirtyLog)?;
                if names == 0
                    || names != u64::from(inode.nlink)
                    || nbytes != inode.size
                    || raw.len() != INODE_ITEM_SIZE
                {
                    return Err(Error::DirtyLog);
                }
                raw[crate::inode::offsets::NBYTES..crate::inode::offsets::NBYTES + 8]
                    .copy_from_slice(&nbytes.to_le_bytes());
                edits.push(ItemEdit::Put(OwnedItem {
                    key: DiskKey {
                        objectid: ino,
                        key_type: INODE_ITEM_KEY,
                        offset: 0,
                    },
                    data: raw,
                }));
            }
            // Each parent's size counts its new names twice, once for each
            // of its two indexes, as a directory's size does.
            for (parent, added) in grown {
                let at = DiskKey {
                    objectid: parent,
                    key_type: INODE_ITEM_KEY,
                    offset: 0,
                };
                let mut raw = tree
                    .search(self.fs_tree_root, &at)?
                    .ok_or(Error::DirtyLog)?
                    .data;
                use crate::inode::offsets::SIZE;
                let size = le64(&raw, SIZE).checked_add(added).ok_or(Error::DirtyLog)?;
                raw[SIZE..SIZE + 8].copy_from_slice(&size.to_le_bytes());
                edits.push(ItemEdit::Put(OwnedItem { key: at, data: raw }));
            }
            for item in &top.items {
                if created.contains(&item.key.objectid) {
                    continue;
                }
                // The digests of logged data, checked below against the
                // bytes they cover.
                if (item.key.objectid, item.key.key_type) == (EXTENT_CSUM_OBJECTID, EXTENT_CSUM_KEY)
                {
                    logged_csums.push(item);
                    continue;
                }
                let committed = tree
                    .search(self.fs_tree_root, &item.key)?
                    .ok_or(Error::DirtyLog)?;
                match item.key.key_type {
                    INODE_ITEM_KEY if item.key.offset == 0 => {
                        if item.data.len() != INODE_ITEM_SIZE
                            || committed.data.len() != INODE_ITEM_SIZE
                        {
                            return Err(Error::DirtyLog);
                        }
                        let before = Inode::parse(&committed.data, item.key.objectid)?;
                        let after = Inode::parse(&item.data, item.key.objectid)?;
                        let committed_extents =
                            tree.find_all(self.fs_tree_root, before.ino, EXTENT_DATA_KEY)?;
                        let logged_extents: Vec<_> = top.extents(before.ino).collect();
                        // An empty file logs no data; an inline file logs
                        // its one extent at offset 0, checked below.
                        let extents_match = if before.size == 0 {
                            before.nbytes == 0
                                && committed_extents.is_empty()
                                && logged_extents.is_empty()
                        } else {
                            committed_extents.len() == 1
                                && logged_extents.len() == 1
                                && logged_extents[0].key.offset == 0
                        };
                        // The kernel logs an inode's `nbytes` as 0 and
                        // replay keeps the committed count, adjusting it
                        // only for extents the log changes; a same-length
                        // rewrite changes none (fixture
                        // btrfs-dirty-log-inline: committed 33, logged 0).
                        if !before.is_regular_file()
                            || !after.is_regular_file()
                            || before.size != after.size
                            || (after.nbytes != before.nbytes && after.nbytes != 0)
                            || !extents_match
                            || before.generation != after.generation
                            || before.ino < crate::inode::FIRST_FREE_OBJECTID
                            || before.nlink == 0
                            || before.nlink != after.nlink
                            || before.flags != after.flags
                            || before.rdev != after.rdev
                            || before.block_group != after.block_group
                        {
                            return Err(Error::DirtyLog);
                        }
                        let mut data = item.data.clone();
                        data[crate::inode::offsets::NBYTES..crate::inode::offsets::NBYTES + 8]
                            .copy_from_slice(&before.nbytes.to_le_bytes());
                        edits.push(ItemEdit::Put(OwnedItem {
                            key: item.key,
                            data,
                        }));
                    }
                    // A same-length rewrite of the committed inline extent,
                    // of an inode whose metadata is in this log and whose
                    // size is that extent's length.
                    EXTENT_DATA_KEY
                        if item.key.offset == 0
                            && same_length_inline(&committed.data, &item.data)
                            && top.inode(item.key.objectid)?.is_some_and(|i| {
                                i.size == (item.data.len() - fe::INLINE_DATA) as u64
                            }) =>
                    {
                        edits.push(ItemEdit::Put(OwnedItem {
                            key: item.key,
                            data: item.data.clone(),
                        }));
                    }
                    // A regular extent in place of the committed one over
                    // the same range, of an inode whose metadata is in this
                    // log (#375): the kernel's `replay_one_extent` for a
                    // whole-extent overwrite. The new extent is one no
                    // record names yet, as `btrfs_alloc_logged_file_extent`
                    // finds it; the committed one loses this item's
                    // reference, and is released when that was its last.
                    EXTENT_DATA_KEY => {
                        let ino = item.key.objectid;
                        let (Some((old, old_len)), Some((new, new_len))) = (
                            whole_regular_extent(&committed.data),
                            whole_regular_extent(&item.data),
                        ) else {
                            return Err(Error::DirtyLog);
                        };
                        if old == new || old_len != new_len || top.inode(ino)?.is_none() {
                            return Err(Error::DirtyLog);
                        }
                        let inode_key = DiskKey {
                            objectid: ino,
                            key_type: INODE_ITEM_KEY,
                            offset: 0,
                        };
                        let inode = tree
                            .search(self.fs_tree_root, &inode_key)?
                            .ok_or(Error::DirtyLog)?;
                        let summed = Inode::parse(&inode.data, ino)?.flags & INODE_NODATASUM == 0;
                        let extent_root = self.tree_root(objectid::EXTENT_TREE)?;
                        let record = |bytenr: u64, len: u64| DiskKey {
                            objectid: bytenr,
                            key_type: EXTENT_ITEM,
                            offset: len,
                        };
                        if tree.search(extent_root, &record(new, new_len))?.is_some() {
                            return Err(Error::DirtyLog);
                        }
                        let before = tree
                            .search(extent_root, &record(old, old_len))?
                            .ok_or(Error::DirtyLog)?
                            .data;
                        // References kept as items of their own, and an
                        // extent a snapshot may still read through a tree
                        // block it shares, are not counted in the inline
                        // references this edits.
                        for kind in [EXTENT_DATA_REF, SHARED_DATA_REF] {
                            if !tree.find_all(extent_root, old, kind)?.is_empty() {
                                return Err(Error::DirtyLog);
                            }
                        }
                        if before.len() < 24 || le64(&before, 8) <= last_snapshot {
                            return Err(Error::DirtyLog);
                        }
                        // The item's extent offset is 0, so its reference
                        // names its own key offset.
                        let ref_offset = item.key.offset;
                        let mut fields = Vec::with_capacity(24);
                        for v in [objectid::FS_TREE, ino, ref_offset] {
                            fields.extend_from_slice(&v.to_le_bytes());
                        }
                        let after = if le64(&before, 0) == 1 {
                            let refs = backref::inline_refs(EXTENT_ITEM, &before)?;
                            let ours = refs.len() == 1
                                && refs[0].kind == EXTENT_DATA_REF
                                && refs[0].payload[..24] == fields[..]
                                && refs[0].payload[24..28] == 1u32.to_le_bytes();
                            if !ours {
                                return Err(Error::DirtyLog);
                            }
                            released.push((old, old + old_len));
                            None
                        } else {
                            Some(
                                backref::drop_inline_data_ref(
                                    EXTENT_ITEM,
                                    &before,
                                    objectid::FS_TREE,
                                    ino,
                                    ref_offset,
                                )?
                                .ok_or(Error::DirtyLog)?,
                            )
                        };
                        records.push(RecordEdit {
                            bytenr: old,
                            len: old_len,
                            before: Some(before),
                            after,
                        });
                        let mut body = Vec::with_capacity(53);
                        for v in [1, generation, EXTENT_FLAG_DATA] {
                            body.extend_from_slice(&v.to_le_bytes());
                        }
                        body.push(EXTENT_DATA_REF);
                        body.extend_from_slice(&fields);
                        body.extend_from_slice(&1u32.to_le_bytes());
                        records.push(RecordEdit {
                            bytenr: new,
                            len: new_len,
                            before: None,
                            after: Some(body),
                        });
                        news.push((new, new_len, summed));
                        edits.push(ItemEdit::Put(OwnedItem {
                            key: item.key,
                            data: item.data.clone(),
                        }));
                    }
                    // References may only repeat the already committed
                    // bytes of an inode whose metadata is in this log.
                    INODE_REF_KEY
                        if committed.data == item.data
                            && top.inodes().contains(&item.key.objectid) => {}
                    _ => return Err(Error::DirtyLog),
                }
            }
            if edits.is_empty() {
                return Err(Error::DirtyLog);
            }
            // Log blocks have no extent-tree allocation. Pin all of
            // them until the same commit both installs the inode items
            // and clears the log, preserving recovery after a crash.
            let mut stack = vec![log.log_root, top.root];
            let mut seen = BTreeSet::new();
            while let Some(at) = stack.pop() {
                if !seen.insert(at) {
                    return Err(Error::DirtyLog);
                }
                let block = tree.read_block(at)?;
                pins.push((at, u64::from(self.sb.nodesize)));
                if let Some(children) = block.body.key_ptrs() {
                    stack.extend(children.iter().map(|child| child.blockptr));
                }
            }
        }
        // The new extents' digests, from the bytes already on disk, and
        // the released extents' removed. The log carries the digests the
        // kernel computed; they must be exactly these, sector for sector,
        // or the data did not reach the disk as logged.
        let mut contents = Vec::new();
        for &(new, len, summed) in &news {
            let mut buf = vec![0u8; usize::try_from(len).map_err(|_| Error::DirtyLog)?];
            Self::read_logical_pool(&self.device, &self.devices, &self.map, new, &mut buf)?;
            if summed {
                contents.push((new, buf));
            }
        }
        let csum_edits = if records.is_empty() {
            Vec::new()
        } else {
            self.csum_edits(&released, &contents, true)?
        };
        let size = self.sb.csum_type.digest_len();
        let sector = u64::from(self.sb.sectorsize);
        let sectors =
            |items: &mut dyn Iterator<Item = (u64, &[u8])>| -> Result<BTreeMap<u64, Vec<u8>>> {
                let mut out = BTreeMap::new();
                for (start, data) in items {
                    if data.len() % size != 0 {
                        return Err(Error::DirtyLog);
                    }
                    for (i, digest) in data.chunks(size).enumerate() {
                        out.insert(start + i as u64 * sector, digest.to_vec());
                    }
                }
                Ok(out)
            };
        let in_new = |at: u64| {
            contents
                .iter()
                .any(|(n, b)| (*n..*n + b.len() as u64).contains(&at))
        };
        let logged = sectors(&mut logged_csums.iter().map(|i| (i.key.offset, &i.data[..])))?;
        let computed = sectors(&mut csum_edits.iter().filter_map(|e| match e {
            ItemEdit::Put(item) if in_new(item.key.offset) => {
                Some((item.key.offset, &item.data[..]))
            }
            _ => None,
        }))?;
        if logged != computed {
            return Err(Error::DirtyLog);
        }
        self.pinned.extend(pins);
        self.commit_write_with(
            DataWrite {
                root: objectid::FS_TREE,
                edits,
                csum_edits,
                records,
                ..Default::default()
            },
            generation,
            true,
        )
    }

    /// Open a volume read-only whatever its log holds, reading the
    /// committed trees and leaving the log unreplayed — the kernel's
    /// `-o ro,nologreplay`.
    ///
    /// An opt-in, never a fallback: on a volume with a log, what was
    /// `fsync`ed after the last commit is not in the trees this reads,
    /// so a file can read back older than an application was told it
    /// durably was. [`Filesystem::log`] reads what the log holds.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::mount`], except that a non-empty log is not
    /// refused.
    pub fn mount_ignoring_log(device: Arc<dyn BlockRead>) -> Result<Self> {
        let (mut sb, copy) = crate::superblock::read_superblock(&*device)?;
        let log_root = sb.log_root;
        let log_root_level = sb.log_root_level;
        sb.log_root = 0;
        let mut fs = Self::open_known(device, sb, copy)?;
        // The superblock this handle reports is the one on the device.
        fs.sb.log_root = log_root;
        fs.sb.log_root_level = log_root_level;
        Ok(fs)
    }

    /// Every log tree the volume holds, or `None` when its log is empty.
    ///
    /// # Errors
    ///
    /// A read or parse failure on a log block, and
    /// [`Error::BadSuperblock`] for a log root tree item that names no
    /// root.
    pub fn log(&self) -> Result<Option<LogContents>> {
        if self.sb.log_root == 0 {
            return Ok(None);
        }
        let reader = self.pool_reader();
        let tree = reader.tree();

        let mut roots: Vec<(u64, u64)> = Vec::new();
        tree.for_each(self.sb.log_root, &mut |key: &DiskKey, data: &[u8]| {
            if key.objectid == TREE_LOG_OBJECTID && key.key_type == ROOT_ITEM_KEY {
                let at = data
                    .get(root_item::BYTENR..root_item::BYTENR + 8)
                    .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
                    .ok_or_else(|| {
                        Error::BadSuperblock(format!(
                            "the log root tree's item for subvolume {} is {} bytes, too short \
                             to name a root",
                            key.offset,
                            data.len()
                        ))
                    })?;
                roots.push((key.offset, at));
            }
            Ok(true)
        })?;

        let mut trees = Vec::with_capacity(roots.len());
        for (subvolume, root) in roots {
            let mut items = Vec::new();
            tree.for_each(root, &mut |key: &DiskKey, data: &[u8]| {
                items.push(LogItem {
                    key: *key,
                    data: data.to_vec(),
                });
                Ok(true)
            })?;
            trees.push(LoggedTree {
                subvolume,
                root,
                items,
            });
        }
        Ok(Some(LogContents {
            log_root: self.sb.log_root,
            trees,
        }))
    }

    /// The contents of `ino` as the log holds them: its logged size,
    /// filled from its logged extents, with holes and preallocated ranges
    /// reading as zeros.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the log holds no inode `ino`, and
    /// [`Error::UnsupportedFeature`] for a compressed or encrypted
    /// extent, which this does not decode.
    pub fn logged_file(&self, tree: &LoggedTree, ino: u64) -> Result<Vec<u8>> {
        let inode = tree.inode(ino)?.ok_or(Error::NotFound)?;
        let size = usize::try_from(inode.size).map_err(|_| {
            Error::UnsupportedFeature(format!("a logged file of {} bytes", inode.size))
        })?;
        let mut out = vec![0u8; size];
        for item in tree.extents(ino) {
            let d = &item.data;
            let start = item.key.offset;
            if d.len() <= fe::TYPE {
                return Err(Error::BadSuperblock(format!(
                    "logged extent item of inode {ino} at {start} is {} bytes",
                    d.len()
                )));
            }
            if d[fe::COMPRESSION] != 0 || d[fe::ENCRYPTION] != 0 {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}'s logged extent at {start} is compressed or encrypted, which \
                     reading the log does not decode"
                )));
            }
            let (bytes, len) = match d[fe::TYPE] {
                0 => {
                    let data = d[fe::INLINE_DATA..].to_vec();
                    let len = data.len() as u64;
                    (Some(data), len)
                }
                1 if d.len() >= fe::REGULAR_SIZE && le64(d, fe::DISK_BYTENR) != 0 => {
                    let len = le64(d, fe::NUM_BYTES);
                    let logical = le64(d, fe::DISK_BYTENR) + le64(d, fe::OFFSET);
                    let n = usize::try_from(len).map_err(|_| {
                        Error::UnsupportedFeature(format!("a logged extent of {len} bytes"))
                    })?;
                    let mut buf = vec![0u8; n];
                    Self::read_logical_pool(
                        &self.device,
                        &self.devices,
                        &self.map,
                        logical,
                        &mut buf,
                    )?;
                    (Some(buf), len)
                }
                // A hole, or preallocated space: zeros.
                1 | 2 if d.len() >= fe::REGULAR_SIZE => (None, le64(d, fe::NUM_BYTES)),
                other => {
                    return Err(Error::BadSuperblock(format!(
                        "inode {ino}'s logged extent at {start} has type {other}"
                    )))
                }
            };
            let from = usize::try_from(start).unwrap_or(usize::MAX).min(size);
            let to = usize::try_from(start.saturating_add(len))
                .unwrap_or(usize::MAX)
                .min(size);
            match bytes {
                Some(b) => out[from..to].copy_from_slice(&b[..to - from]),
                None => out[from..to].fill(0),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(objectid: u64, key_type: u8, offset: u64, data: Vec<u8>) -> LogItem {
        LogItem {
            key: DiskKey {
                objectid,
                key_type,
                offset,
            },
            data,
        }
    }

    #[test]
    fn a_logged_tree_lists_only_inode_items_as_inodes() {
        let tree = LoggedTree {
            subvolume: 5,
            root: 0,
            items: vec![
                item(256, INODE_ITEM_KEY, 0, vec![0; 160]),
                item(256, DIR_LOG_INDEX_KEY, 2, vec![0; 8]),
                item(257, INODE_ITEM_KEY, 0, vec![0; 160]),
                item(257, EXTENT_DATA_KEY, 0, vec![0; 21]),
            ],
        };
        assert_eq!(tree.inodes(), vec![256, 257]);
        let mut refs = 2u64.to_le_bytes().to_vec();
        refs.extend(3u16.to_le_bytes());
        refs.extend(b"abc");
        let tree = LoggedTree {
            items: vec![item(300, INODE_REF_KEY, 256, refs)],
            ..tree
        };
        assert_eq!(tree.inode_named(256, b"abc"), Some(300));
        assert_eq!(tree.inode_named(256, b"ab"), None);
        assert_eq!(tree.inode_named(257, b"abc"), None);
        assert!(tree.inode(258).unwrap().is_none());
    }

    fn inline(data: &[u8]) -> Vec<u8> {
        let mut d = vec![0u8; fe::INLINE_DATA];
        d[fe::RAM_BYTES..fe::RAM_BYTES + 8].copy_from_slice(&(data.len() as u64).to_le_bytes());
        d.extend_from_slice(data);
        d
    }

    #[test]
    fn only_a_plain_same_length_inline_rewrite_is_replayable() {
        let committed = inline(b"committed");
        assert!(same_length_inline(&committed, &inline(b"rewritten")));
        assert!(
            !same_length_inline(&committed, &inline(b"longer data")),
            "size change"
        );
        for at in [
            fe::COMPRESSION,
            fe::ENCRYPTION,
            fe::OTHER_ENCODING,
            fe::OTHER_ENCODING + 1,
        ] {
            let mut encoded = inline(b"rewritten");
            encoded[at] = 1;
            assert!(
                !same_length_inline(&committed, &encoded),
                "encoding byte {at}"
            );
            assert!(
                !same_length_inline(&encoded, &inline(b"rewritten")),
                "committed byte {at}"
            );
        }
        let mut external = inline(b"rewritten");
        external[fe::TYPE] = 1;
        assert!(
            !same_length_inline(&committed, &external),
            "external extent"
        );
        let mut ram = inline(b"rewritten");
        ram[fe::RAM_BYTES] = 3;
        assert!(
            !same_length_inline(&committed, &ram),
            "ram_bytes disagrees with data"
        );
        assert!(
            !same_length_inline(&inline(b""), &inline(b"")),
            "no inline data"
        );
    }
}
