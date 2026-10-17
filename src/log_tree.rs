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

use std::collections::BTreeMap;
use std::sync::Arc;

use fs_core::BlockRead;

use crate::chunk::DiskKey;
use crate::dir::{self, DirEntry, DIR_INDEX_KEY};
use crate::error::{Error, Result};
use crate::fs::{root_item, Filesystem, EXTENT_DATA_KEY, ROOT_ITEM_KEY};
use crate::inode::{FileType, Inode, INODE_ITEM_KEY, INODE_REF_KEY};

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
    /// `INODE_REF` and `INODE_EXTREF` items: what a logged new name, or a
    /// logged inode's name, resolves to.
    pub fn inode_named(&self, parent: u64, name: &[u8]) -> Option<u64> {
        self.items
            .iter()
            .find(|i| match i.key.key_type {
                INODE_REF_KEY => i.key.offset == parent && ref_names(&i.data).any(|n| n == name),
                INODE_EXTREF_KEY => extref_records(&i.data)
                    .unwrap_or_default()
                    .iter()
                    .any(|&(p, _, n)| p == parent && n == name),
                _ => false,
            })
            .map(|i| i.key.objectid)
    }

    /// Whether a logged `INODE_REF` or `INODE_EXTREF` item of `ino` names
    /// it `name` at `index` in directory `parent`.
    fn names_at(&self, ino: u64, parent: u64, index: u64, name: &[u8]) -> bool {
        self.items
            .iter()
            .filter(|i| i.key.objectid == ino)
            .filter_map(|i| name_records(&i.key, &i.data))
            .any(|records| records.contains(&(parent, index, name)))
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

/// `BTRFS_INODE_EXTREF_KEY`: names of an inode kept outside its
/// `INODE_REF` items, as the kernel stores them once the `INODE_REF` for
/// a parent has grown to the largest item a leaf holds, filed under a
/// hash of each name's parent and name (#407).
pub const INODE_EXTREF_KEY: u8 = 13;

/// The records of an `INODE_EXTREF` item: each name's parent, its index
/// in the parent, and the name, after an 8-byte parent, an 8-byte index
/// and a 2-byte length. `None` when a record runs past the end.
fn extref_records(data: &[u8]) -> Option<Vec<(u64, u64, &[u8])>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let parent = u64::from_le_bytes(data.get(pos..pos + 8)?.try_into().ok()?);
        let index = u64::from_le_bytes(data.get(pos + 8..pos + 16)?.try_into().ok()?);
        let len = usize::from(u16::from_le_bytes(
            data.get(pos + 16..pos + 18)?.try_into().ok()?,
        ));
        out.push((parent, index, data.get(pos + 18..pos + 18 + len)?));
        pos += 18 + len;
    }
    Some(out)
}

/// The key offset an extended reference to `name` in `parent` is filed
/// under: the kernel's `btrfs_extref_hash`, a CRC-32C of the name seeded
/// with the parent's low 32 bits, with no inversion at either end (see
/// [`dir::name_hash`]).
fn extref_hash(parent: u64, name: &[u8]) -> u64 {
    u64::from(!crc32c::crc32c_append(!(parent as u32), name))
}

/// The names a reference item of either kind holds, as (parent, index,
/// name): an `INODE_REF` names its parent in its key, an `INODE_EXTREF`
/// in each record. `None` for a record that runs past the end, or an
/// extended record not filed under the hash of its own parent and name.
fn name_records<'a>(key: &DiskKey, data: &'a [u8]) -> Option<Vec<(u64, u64, &'a [u8])>> {
    match key.key_type {
        INODE_REF_KEY => Some(
            ref_records(data)?
                .into_iter()
                .map(|(index, name)| (key.offset, index, name))
                .collect(),
        ),
        INODE_EXTREF_KEY => {
            let records = extref_records(data)?;
            records
                .iter()
                .all(|&(parent, _, name)| extref_hash(parent, name) == key.offset)
                .then_some(records)
        }
        _ => None,
    }
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// Whether a logged file extent item only rewrites the bytes of a
/// committed inline extent at the same length: both inline, neither
/// compressed, encrypted or otherwise encoded, and each one's
/// `ram_bytes` naming exactly the data it carries.
#[cfg(test)]
fn same_length_inline(committed: &[u8], logged: &[u8]) -> bool {
    plain_inline(committed) && plain_inline(logged) && committed.len() == logged.len()
}

/// Whether a file extent item is inline, unencrypted and not otherwise
/// encoded, and either compressed by an algorithm the format defines or
/// plain (#378). A plain one's `ram_bytes` must name exactly the data it
/// carries; a compressed one's names the decoded length.
fn inline_extent(d: &[u8]) -> bool {
    d.len() > fe::INLINE_DATA
        && d[fe::TYPE] == 0
        && d[fe::COMPRESSION] <= 3
        && d[fe::ENCRYPTION] == 0
        && d[fe::OTHER_ENCODING..fe::TYPE] == [0, 0]
        && (d[fe::COMPRESSION] != 0 || plain_inline(d))
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

/// The extent a regular or preallocated file extent item names, and the
/// reference it is to it: the extent's address and length, and the file
/// offset at which the extent's first byte would sit -- the item's key
/// offset less its offset into the extent, wrapping as the kernel's
/// subtraction does. `None` for inline data and holes.
fn file_ref(at: u64, d: &[u8]) -> Option<(u64, u64, u64)> {
    let regular = d.len() == fe::REGULAR_SIZE && matches!(d[fe::TYPE], 1 | 2);
    let bytenr = regular
        .then(|| le64(d, fe::DISK_BYTENR))
        .filter(|&b| b != 0)?;
    Some((
        bytenr,
        le64(d, fe::DISK_NUM_BYTES),
        at.wrapping_sub(le64(d, fe::OFFSET)),
    ))
}

/// What a file extent item counts in its inode's `nbytes`: an inline
/// extent's decoded length, a regular or preallocated item's length, and
/// nothing for a hole.
fn counted_bytes(d: &[u8]) -> u64 {
    match d.get(fe::TYPE) {
        Some(0) if d.len() > fe::INLINE_DATA => le64(d, fe::RAM_BYTES),
        Some(1 | 2) if d.len() == fe::REGULAR_SIZE && le64(d, fe::DISK_BYTENR) != 0 => {
            le64(d, fe::NUM_BYTES)
        }
        _ => 0,
    }
}

/// The bytes of the file a file extent item covers from its key offset:
/// an inline extent its decoded length, any other its `num_bytes`.
fn item_span(d: &[u8]) -> Result<u64> {
    match d.get(fe::TYPE) {
        Some(0) if inline_extent(d) => Ok(le64(d, fe::RAM_BYTES).max(1)),
        Some(1 | 2) if d.len() == fe::REGULAR_SIZE && le64(d, fe::NUM_BYTES) != 0 => {
            Ok(le64(d, fe::NUM_BYTES))
        }
        _ => Err(Error::DirtyLog),
    }
}

/// A regular, preallocated or hole item cut to the `num` bytes that begin
/// `skip` bytes into it. A compressed extent is decoded whole, so a window
/// into part of one is refused.
fn cut_item(d: &[u8], skip: u64, num: u64) -> Result<Vec<u8>> {
    if d.len() != fe::REGULAR_SIZE || !matches!(d[fe::TYPE], 1 | 2) || d[fe::COMPRESSION] != 0 {
        return Err(Error::DirtyLog);
    }
    let offset = le64(d, fe::OFFSET)
        .checked_add(skip)
        .ok_or(Error::DirtyLog)?;
    if le64(d, fe::DISK_BYTENR) != 0
        && offset.checked_add(num).ok_or(Error::DirtyLog)? > le64(d, fe::RAM_BYTES)
    {
        return Err(Error::DirtyLog);
    }
    let mut out = d.to_vec();
    out[fe::OFFSET..fe::OFFSET + 8].copy_from_slice(&offset.to_le_bytes());
    out[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&num.to_le_bytes());
    Ok(out)
}

/// The file extent items, by key offset, a file is left with when its
/// inode is logged in full: the `committed` items less the ranges the
/// `logged` items cover -- a committed item covered in part keeps its
/// head and its tail -- with the logged items in their place, and nothing
/// at or past `end`, the logged size rounded up to a sector. An inline
/// extent is a file's only item, so it is replaced only from offset 0,
/// and a file left with one beside other items is refused.
fn reconcile(
    committed: &BTreeMap<u64, Vec<u8>>,
    logged: &BTreeMap<u64, Vec<u8>>,
    end: u64,
) -> Result<BTreeMap<u64, Vec<u8>>> {
    let mut items = committed.clone();
    for (&at, item) in logged {
        if item[fe::TYPE] == 0 && (!inline_extent(item) || at != 0) {
            return Err(Error::DirtyLog);
        }
        let stop = at.checked_add(item_span(item)?).ok_or(Error::DirtyLog)?;
        let mut overlapped = Vec::new();
        for (&k, d) in &items {
            let e = k.checked_add(item_span(d)?).ok_or(Error::DirtyLog)?;
            if k < stop && at < e {
                overlapped.push((k, e));
            }
        }
        for (k, e) in overlapped {
            let d = items.remove(&k).expect("an overlapped item is there");
            if d[fe::TYPE] == 0 {
                if at != 0 {
                    return Err(Error::DirtyLog);
                }
                continue;
            }
            if k < at {
                items.insert(k, cut_item(&d, 0, at - k)?);
            }
            if e > stop {
                items.insert(stop, cut_item(&d, stop - k, e - stop)?);
            }
        }
        items.insert(at, item.clone());
    }
    let keys: Vec<u64> = items.keys().copied().collect();
    for k in keys {
        if k >= end {
            items.remove(&k);
            continue;
        }
        let d = &items[&k];
        if d[fe::TYPE] != 0 {
            let e = k.checked_add(item_span(d)?).ok_or(Error::DirtyLog)?;
            if e > end {
                let cut = cut_item(d, 0, end - k)?;
                items.insert(k, cut);
            }
        }
    }
    if items.len() > 1 && items.values().any(|d| d[fe::TYPE] == 0) {
        return Err(Error::DirtyLog);
    }
    Ok(items)
}

/// The references to data extents that replay adds and drops, by extent
/// (address, length) and by reference (inode, offset), with whether a
/// file naming the extent is checksummed.
#[derive(Default)]
struct Ledger {
    extents: BTreeMap<(u64, u64), LedgerExtent>,
}

#[derive(Default)]
struct LedgerExtent {
    /// By (subvolume, inode, offset).
    refs: BTreeMap<(u64, u64, u64), i64>,
    summed: bool,
    /// The latest snapshot of any subvolume naming it: an extent from at
    /// or before it may be shared through a snapshot's tree blocks.
    last_snapshot: u64,
}

impl Ledger {
    fn change(
        &mut self,
        r: (u64, u64, u64),
        subvol: u64,
        ino: u64,
        delta: i64,
        summed: bool,
        last_snapshot: u64,
    ) -> Result<()> {
        let (bytenr, len, offset) = r;
        if self.extents.keys().any(|&(b, l)| b == bytenr && l != len) {
            return Err(Error::DirtyLog);
        }
        let e = self.extents.entry((bytenr, len)).or_default();
        *e.refs.entry((subvol, ino, offset)).or_default() += delta;
        e.summed |= summed;
        e.last_snapshot = e.last_snapshot.max(last_snapshot);
        Ok(())
    }
}

impl Filesystem {
    /// The `last_snapshot` of subvolume `objectid`'s `ROOT_ITEM` (#380).
    fn root_last_snapshot(&self, objectid: u64) -> Result<u64> {
        if objectid == crate::chunk::objectid::FS_TREE {
            return self.fs_tree_last_snapshot();
        }
        let reader = self.pool_reader();
        let mut found = None;
        reader
            .tree()
            .for_each(self.sb.root, &mut |key: &DiskKey, data: &[u8]| {
                if key.objectid == objectid && key.key_type == ROOT_ITEM_KEY {
                    found = data
                        .get(root_item::LAST_SNAPSHOT..root_item::LAST_SNAPSHOT + 8)
                        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")));
                    return Ok(false);
                }
                Ok(true)
            })?;
        found.ok_or(Error::DirtyLog)
    }

    /// Replay the log of every subvolume the log root tree names (#380),
    /// as the kernel does for the shapes below, in one transaction. Everything is validated before the first write,
    /// and anything else is refused with [`Error::DirtyLog`] until its
    /// replay has independent oracle proof (#266).
    ///
    /// - **Files** whose inode the log holds in full (#364, #375, #376,
    ///   #378): the logged extent items replace whatever committed items
    ///   cover their ranges -- a committed item they cover in part keeps
    ///   its head and its tail -- and the file is cut at its logged size,
    ///   rounded up to a sector, as the logged inode item is the truth of
    ///   it. Its `nbytes` is counted from what is left. Its references,
    ///   in `INODE_REF` and `INODE_EXTREF` items alike (#407), keep every
    ///   committed name and may add hard links (#406), each in a directory
    ///   the log speaks for, its link count raised by one for each.
    /// - **Extents**: every file item that names an extent is one
    ///   reference to it. The references replay adds and drops are applied
    ///   to each extent's record: an extent nothing recorded yet is
    ///   recorded, as `btrfs_alloc_logged_file_extent` finds it, and one
    ///   left with no reference is released with its checksums. The
    ///   logged checksums of new extents must be exactly the digests of
    ///   the bytes on disk.
    /// - **Inodes the log creates** are inserted, named from their
    ///   references, or from the logged entries of a directory the log
    ///   speaks for. A directory the log creates (#403) is named only
    ///   that way, and holds the names the log gives it. A symbolic link
    ///   the log creates (#404) holds its target as logged, in one plain
    ///   inline extent. A device, FIFO or socket node the log creates
    ///   (#405) holds no data, and keeps its type and device number as
    ///   logged. Each name of a created inode is typed as the inode is.
    /// - **Directories the log speaks for** (#377): in each logged index
    ///   range the log's names are the directory's names. A committed name
    ///   the log lacks is removed; its inode, if the log does not hold it
    ///   and that was its last name, is deleted with its data. A logged
    ///   name the directory lacks is added.
    /// - **Inodes logged only to say they exist** (generation 0) keep
    ///   their committed item; their logged references are their names.
    pub(crate) fn replay_inode_only_log(&mut self) -> Result<()> {
        use crate::backref::{self, EXTENT_DATA_REF, SHARED_DATA_REF};
        use crate::chunk::{key_type::EXTENT_ITEM, objectid};
        use crate::csum::{EXTENT_CSUM_KEY, EXTENT_CSUM_OBJECTID};
        use crate::extent_write::EXTENT_FLAG_DATA;
        use crate::inode::{offsets as io, FIRST_FREE_OBJECTID, INODE_ITEM_SIZE, INODE_NODATASUM};
        use crate::leaf_edit::OwnedItem;
        use crate::transaction::{DataWrite, ItemEdit, RecordEdit};
        use std::collections::{BTreeMap, BTreeSet};

        let log = self.log()?.ok_or(Error::DirtyLog)?;
        // The log of every subvolume the log root tree names, the top
        // level's or another's (#380), replayed in one transaction.
        if log.trees.is_empty()
            || log
                .trees
                .iter()
                .any(|t| t.subvolume != objectid::FS_TREE && t.subvolume < FIRST_FREE_OBJECTID)
        {
            return Err(Error::DirtyLog);
        }
        self.require_writable_top_level()?;
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the log replay generation overflows".into())
        })?;
        let sector = u64::from(self.sb.sectorsize);
        let key = |objectid: u64, key_type: u8, offset: u64| DiskKey {
            objectid,
            key_type,
            offset,
        };
        let put = |key: DiskKey, data: Vec<u8>| ItemEdit::Put(OwnedItem { key, data });

        let mut fs_edits: Vec<ItemEdit> = Vec::new();
        let mut tree_edits: Vec<(u64, ItemEdit)> = Vec::new();
        let mut pins = Vec::new();
        let mut ledger = Ledger::default();
        let mut logged_csums: Vec<&LogItem> = Vec::new();
        {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let mut roots = 0;
            tree.for_each(log.log_root, &mut |key, _| {
                if key.objectid != TREE_LOG_OBJECTID
                    || key.key_type != ROOT_ITEM_KEY
                    || !log.trees.iter().any(|t| t.subvolume == key.offset)
                {
                    return Err(Error::DirtyLog);
                }
                roots += 1;
                Ok(true)
            })?;
            if roots != log.trees.len() {
                return Err(Error::DirtyLog);
            }

            for top in &log.trees {
                let subvol = top.subvolume;
                let root = self.tree_root(subvol)?;
                let last_snapshot = self.root_last_snapshot(subvol)?;
                let mut edits: Vec<ItemEdit> = Vec::new();
                // What each logged inode is to replay.
                let logged_inodes = top.inodes();
                let mut created: BTreeSet<u64> = BTreeSet::new();
                let mut files: BTreeSet<u64> = BTreeSet::new();
                let mut exists_only: BTreeSet<u64> = BTreeSet::new();
                let mut dirs: BTreeSet<u64> = BTreeSet::new();
                // Directories the log creates (#403).
                let mut new_dirs: BTreeSet<u64> = BTreeSet::new();
                // Symbolic links the log creates (#404), each also in `created`.
                let mut links: BTreeSet<u64> = BTreeSet::new();
                // Device, FIFO and socket nodes the log creates (#405), each
                // also in `created`.
                let mut nodes: BTreeSet<u64> = BTreeSet::new();
                for &ino in &logged_inodes {
                    let logged = top.inode(ino)?.ok_or(Error::DirtyLog)?;
                    if ino < FIRST_FREE_OBJECTID {
                        return Err(Error::DirtyLog);
                    }
                    match tree.search(root, &key(ino, INODE_ITEM_KEY, 0))? {
                        None if logged.is_regular_file() && ino >= FIRST_FREE_OBJECTID => {
                            created.insert(ino);
                        }
                        None if logged.is_symlink() => {
                            created.insert(ino);
                            links.insert(ino);
                        }
                        None if matches!(
                            logged.file_type(),
                            Some(
                                FileType::CharDevice
                                    | FileType::BlockDevice
                                    | FileType::Fifo
                                    | FileType::Socket
                            )
                        ) =>
                        {
                            created.insert(ino);
                            nodes.insert(ino);
                        }
                        None if logged.is_dir() => {
                            new_dirs.insert(ino);
                        }
                        Some(c) => {
                            let committed = Inode::parse(&c.data, ino)?;
                            if committed.is_dir() && logged.is_dir() {
                                dirs.insert(ino);
                            } else if committed.is_regular_file() && logged.is_regular_file() {
                                if logged.generation == 0 {
                                    exists_only.insert(ino);
                                } else {
                                    files.insert(ino);
                                }
                            } else {
                                return Err(Error::DirtyLog);
                            }
                        }
                        None => return Err(Error::DirtyLog),
                    }
                }
                for item in &top.items {
                    if (item.key.objectid, item.key.key_type)
                        == (EXTENT_CSUM_OBJECTID, EXTENT_CSUM_KEY)
                    {
                        logged_csums.push(item);
                    } else if !logged_inodes.contains(&item.key.objectid) {
                        return Err(Error::DirtyLog);
                    }
                }

                // The one logged entry of directory `dir` at `index`, if the
                // log holds one.
                let logged_entry = |dir: u64, index: u64| -> Result<Option<DirEntry>> {
                    let Some(item) = top
                        .items
                        .iter()
                        .find(|i| i.key == key(dir, DIR_INDEX_KEY, index))
                    else {
                        return Ok(None);
                    };
                    let mut entries = dir::parse_dir_items(&item.data)?;
                    if entries.len() != 1 {
                        return Err(Error::DirtyLog);
                    }
                    Ok(entries.pop())
                };

                // Each file's data, whether the log creates the file or holds
                // an existing one in full.
                for &ino in files.iter().chain(created.iter()) {
                    let logged_item = top
                        .items
                        .iter()
                        .find(|i| i.key == key(ino, INODE_ITEM_KEY, 0))
                        .ok_or(Error::DirtyLog)?;
                    let logged = Inode::parse(&logged_item.data, ino)?;
                    if logged_item.data.len() != INODE_ITEM_SIZE {
                        return Err(Error::DirtyLog);
                    }
                    let committed_items: BTreeMap<u64, Vec<u8>> = tree
                        .find_all(root, ino, EXTENT_DATA_KEY)?
                        .into_iter()
                        .map(|i| (i.key.offset, i.data))
                        .collect();
                    let logged_items: BTreeMap<u64, Vec<u8>> = top
                        .extents(ino)
                        .map(|i| (i.key.offset, i.data.clone()))
                        .collect();
                    if created.contains(&ino) {
                        for kind in [INODE_REF_KEY, EXTENT_DATA_KEY, dir::XATTR_ITEM_KEY] {
                            if !tree.find_all(root, ino, kind)?.is_empty() {
                                return Err(Error::DirtyLog);
                            }
                        }
                    }
                    if links.contains(&ino) {
                        // A link's target is its only extent: plain, inline,
                        // at 0 and exactly its size, as the kernel writes it.
                        let target = logged_items.get(&0).filter(|d| plain_inline(d));
                        if logged_items.len() != 1
                            || logged.size == 0
                            || target.map(|d| le64(d, fe::RAM_BYTES)) != Some(logged.size)
                        {
                            return Err(Error::DirtyLog);
                        }
                    }
                    // A node holds no data: its device number, if it has
                    // one, is in its inode item, which is put as logged.
                    if nodes.contains(&ino) && (!logged_items.is_empty() || logged.size != 0) {
                        return Err(Error::DirtyLog);
                    }
                    let end = logged.size.div_ceil(sector) * sector;
                    let after = reconcile(&committed_items, &logged_items, end)?;
                    let summed = logged.flags & INODE_NODATASUM == 0;
                    for (&at, data) in &committed_items {
                        if !after.contains_key(&at) {
                            edits.push(ItemEdit::Delete(key(ino, EXTENT_DATA_KEY, at)));
                        }
                        if let Some(r) = file_ref(at, data) {
                            ledger.change(r, subvol, ino, -1, summed, last_snapshot)?;
                        }
                    }
                    for (&at, data) in &after {
                        if committed_items.get(&at) != Some(data) {
                            edits.push(put(key(ino, EXTENT_DATA_KEY, at), data.clone()));
                        }
                        if let Some(r) = file_ref(at, data) {
                            ledger.change(r, subvol, ino, 1, summed, last_snapshot)?;
                        }
                    }
                    let nbytes = after.values().map(|d| counted_bytes(d)).sum::<u64>();
                    let mut raw = logged_item.data.clone();
                    if files.contains(&ino) {
                        let c = tree
                            .search(root, &key(ino, INODE_ITEM_KEY, 0))?
                            .ok_or(Error::DirtyLog)?;
                        let before = Inode::parse(&c.data, ino)?;
                        // The kernel logs `nbytes` as 0 and counts it again from
                        // the data; a committed count that disagrees with the
                        // committed items is not a volume this understands.
                        let committed_nbytes = committed_items
                            .values()
                            .map(|d| counted_bytes(d))
                            .sum::<u64>();
                        if before.nbytes != committed_nbytes
                            || (logged.nbytes != 0 && logged.nbytes != before.nbytes)
                            || before.generation != logged.generation
                            || before.nlink == 0
                            || before.flags != logged.flags
                            || before.rdev != logged.rdev
                            || before.block_group != logged.block_group
                        {
                            return Err(Error::DirtyLog);
                        }
                        // Its references, `INODE_REF` and `INODE_EXTREF`
                        // alike (#407), keep every committed name, and may
                        // add names (#406): each one in a committed directory
                        // the log speaks for, whose logged entry at that index
                        // is the name, which the directory pass adds. A renamed
                        // file is replayed through its directory.
                        let mut gained = 0u32;
                        for r in top.items.iter().filter(|i| {
                            i.key.objectid == ino
                                && matches!(i.key.key_type, INODE_REF_KEY | INODE_EXTREF_KEY)
                        }) {
                            let committed = tree.search(root, &r.key)?.map(|c| c.data);
                            let had = match &committed {
                                Some(data) => name_records(&r.key, data).ok_or(Error::DirtyLog)?,
                                None => Vec::new(),
                            };
                            let has = name_records(&r.key, &r.data).ok_or(Error::DirtyLog)?;
                            if had.iter().any(|record| !has.contains(record)) {
                                return Err(Error::DirtyLog);
                            }
                            for &(parent, index, name) in has.iter().filter(|r| !had.contains(r)) {
                                let entry = logged_entry(parent, index)?.ok_or(Error::DirtyLog)?;
                                if !dirs.contains(&parent)
                                    || entry.ino != ino
                                    || entry.name != name
                                    || entry.ftype != logged.file_type()
                                {
                                    return Err(Error::DirtyLog);
                                }
                                gained += 1;
                            }
                            if committed.as_deref() != Some(&r.data[..]) {
                                edits.push(put(r.key, r.data.clone()));
                            }
                        }
                        // Its link count is its committed names and the ones
                        // it gains, each committed name in an `INODE_REF` or
                        // an `INODE_EXTREF`.
                        let nlink = if gained == 0 {
                            before.nlink
                        } else {
                            let mut committed_names = 0u32;
                            for kind in [INODE_REF_KEY, INODE_EXTREF_KEY] {
                                for c in tree.find_all(root, ino, kind)? {
                                    committed_names +=
                                        name_records(&c.key, &c.data).ok_or(Error::DirtyLog)?.len()
                                            as u32;
                                }
                            }
                            if committed_names != before.nlink {
                                return Err(Error::DirtyLog);
                            }
                            before.nlink.checked_add(gained).ok_or(Error::DirtyLog)?
                        };
                        if logged.nlink != nlink {
                            return Err(Error::DirtyLog);
                        }
                    }
                    raw[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
                    edits.push(put(key(ino, INODE_ITEM_KEY, 0), raw));
                    // Extended references are replayed only for a file the
                    // log holds in full; a created one is named from its
                    // `INODE_REF` items alone.
                    for item in top.items.iter().filter(|i| i.key.objectid == ino) {
                        let extref = item.key.key_type == INODE_EXTREF_KEY && files.contains(&ino);
                        if !extref
                            && !matches!(
                                item.key.key_type,
                                INODE_ITEM_KEY
                                    | INODE_REF_KEY
                                    | EXTENT_DATA_KEY
                                    | dir::XATTR_ITEM_KEY
                            )
                        {
                            return Err(Error::DirtyLog);
                        }
                    }
                    // Its extended attributes are the logged set (#379): each
                    // logged item is put as logged, and a committed one the log
                    // does not hold is deleted, as `replay_xattr_deletes` does.
                    let committed_xattrs: BTreeMap<u64, Vec<u8>> = tree
                        .find_all(root, ino, dir::XATTR_ITEM_KEY)?
                        .into_iter()
                        .map(|i| (i.key.offset, i.data))
                        .collect();
                    let logged_xattrs: BTreeMap<u64, &[u8]> = top
                        .items
                        .iter()
                        .filter(|i| i.key.objectid == ino && i.key.key_type == dir::XATTR_ITEM_KEY)
                        .map(|i| (i.key.offset, &i.data[..]))
                        .collect();
                    for &hash in committed_xattrs.keys() {
                        if !logged_xattrs.contains_key(&hash) {
                            edits.push(ItemEdit::Delete(key(ino, dir::XATTR_ITEM_KEY, hash)));
                        }
                    }
                    for (&hash, &data) in &logged_xattrs {
                        if committed_xattrs.get(&hash).map(|c| &c[..]) != Some(data) {
                            edits.push(put(key(ino, dir::XATTR_ITEM_KEY, hash), data.to_vec()));
                        }
                    }
                }

                // Names. A created inode named in a directory the log does not
                // speak for gets its entries from its reference; any other
                // logged reference must match a logged directory entry, or be
                // the committed one.
                let mut grown: BTreeMap<u64, i64> = BTreeMap::new();
                let mut names: BTreeMap<u64, u32> = BTreeMap::new();
                let spoken_for = |d: u64| dirs.contains(&d) || new_dirs.contains(&d);
                for &ino in created.iter().chain(&exists_only).chain(&new_dirs) {
                    let inode = top.inode(ino)?.ok_or(Error::DirtyLog)?;
                    for r in top
                        .items
                        .iter()
                        .filter(|i| i.key.objectid == ino && i.key.key_type == INODE_REF_KEY)
                    {
                        let parent = r.key.offset;
                        if !new_dirs.contains(&parent) {
                            let p = tree
                                .search(root, &key(parent, INODE_ITEM_KEY, 0))?
                                .ok_or(Error::DirtyLog)?;
                            if !Inode::parse(&p.data, parent)?.is_dir() {
                                return Err(Error::DirtyLog);
                            }
                        }
                        for (index, name) in ref_records(&r.data).ok_or(Error::DirtyLog)? {
                            *names.entry(ino).or_default() += 1;
                            if spoken_for(parent) {
                                let entry = logged_entry(parent, index)?.ok_or(Error::DirtyLog)?;
                                if entry.ino != ino
                                    || entry.name != name
                                    || (created.contains(&ino) && entry.ftype != inode.file_type())
                                {
                                    return Err(Error::DirtyLog);
                                }
                                continue;
                            }
                            if new_dirs.contains(&ino) {
                                // A directory named where the log does not
                                // speak for its parent.
                                return Err(Error::DirtyLog);
                            }
                            if exists_only.contains(&ino) {
                                // Named where the log does not speak for the
                                // directory: the name must be the committed one.
                                let committed =
                                    tree.search(root, &r.key)?.ok_or(Error::DirtyLog)?;
                                if !ref_records(&committed.data)
                                    .ok_or(Error::DirtyLog)?
                                    .contains(&(index, name))
                                {
                                    return Err(Error::DirtyLog);
                                }
                                continue;
                            }
                            let by_hash = key(parent, dir::DIR_ITEM_KEY, dir::name_hash(name));
                            let by_index = key(parent, DIR_INDEX_KEY, index);
                            if tree.search(root, &by_hash)?.is_some()
                                || tree.search(root, &by_index)?.is_some()
                            {
                                return Err(Error::DirtyLog);
                            }
                            let ftype = crate::namespace::file_type(&inode);
                            let entry = crate::namespace::dir_entry(ino, generation, ftype, name);
                            edits.push(put(by_hash, entry.clone()));
                            edits.push(put(by_index, entry));
                            *grown.entry(parent).or_default() += 2 * name.len() as i64;
                        }
                        edits.push(put(r.key, r.data.clone()));
                    }
                }
                for &ino in created.iter().chain(&new_dirs) {
                    let logged = top.inode(ino)?.ok_or(Error::DirtyLog)?;
                    if names.get(&ino).copied().unwrap_or(0) != logged.nlink
                        || logged.nlink == 0
                        || (new_dirs.contains(&ino) && logged.nlink != 1)
                    {
                        return Err(Error::DirtyLog);
                    }
                }

                // An inode logged only to say it exists keeps its committed
                // item; its committed references the log replaces are dropped,
                // and its link count is what its names now are.
                for &ino in &exists_only {
                    if top.extents(ino).next().is_some() {
                        return Err(Error::DirtyLog);
                    }
                    let logged_parents: BTreeSet<u64> = top
                        .items
                        .iter()
                        .filter(|i| i.key.objectid == ino && i.key.key_type == INODE_REF_KEY)
                        .map(|i| i.key.offset)
                        .collect();
                    for c in tree.find_all(root, ino, INODE_REF_KEY)? {
                        if !logged_parents.contains(&c.key.offset) {
                            // A name in a directory the log does not mention.
                            *names.entry(ino).or_default() +=
                                ref_records(&c.data).ok_or(Error::DirtyLog)?.len() as u32;
                        }
                    }
                    for item in top.items.iter().filter(|i| i.key.objectid == ino) {
                        if !matches!(item.key.key_type, INODE_ITEM_KEY | INODE_REF_KEY) {
                            return Err(Error::DirtyLog);
                        }
                    }
                    let c = tree
                        .search(root, &key(ino, INODE_ITEM_KEY, 0))?
                        .ok_or(Error::DirtyLog)?;
                    let nlink = names.get(&ino).copied().unwrap_or(0);
                    if nlink == 0 || c.data.len() != INODE_ITEM_SIZE {
                        return Err(Error::DirtyLog);
                    }
                    let mut raw = c.data.clone();
                    raw[io::NLINK..io::NLINK + 4].copy_from_slice(&nlink.to_le_bytes());
                    edits.push(put(key(ino, INODE_ITEM_KEY, 0), raw));
                }

                // Each directory the log speaks for, committed or created.
                let mut removed_inodes: BTreeSet<u64> = BTreeSet::new();
                for &d in dirs.iter().chain(&new_dirs) {
                    let created_dir = new_dirs.contains(&d);
                    let mut ranges = Vec::new();
                    for item in top.items.iter().filter(|i| i.key.objectid == d) {
                        match item.key.key_type {
                            INODE_ITEM_KEY | INODE_REF_KEY | DIR_INDEX_KEY => {}
                            DIR_LOG_INDEX_KEY if item.data.len() == 8 => {
                                ranges.push((item.key.offset, le64(&item.data, 0)));
                            }
                            _ => return Err(Error::DirtyLog),
                        }
                    }
                    if created_dir {
                        // Nothing of a directory the log creates is committed;
                        // its references were put with the names above.
                        for kind in [
                            INODE_REF_KEY,
                            dir::DIR_ITEM_KEY,
                            DIR_INDEX_KEY,
                            dir::XATTR_ITEM_KEY,
                            EXTENT_DATA_KEY,
                        ] {
                            if !tree.find_all(root, d, kind)?.is_empty() {
                                return Err(Error::DirtyLog);
                            }
                        }
                    }
                    for r in top
                        .items
                        .iter()
                        .filter(|i| i.key.objectid == d && i.key.key_type == INODE_REF_KEY)
                    {
                        if !created_dir
                            && tree.search(root, &r.key)?.map(|c| c.data) != Some(r.data.clone())
                        {
                            return Err(Error::DirtyLog);
                        }
                    }
                    let in_range =
                        |index: u64| ranges.iter().any(|&(a, b)| a <= index && index <= b);
                    let mut delta: i64 = 0;
                    // Committed names in a logged range that the log lacks.
                    for c in tree.find_all(root, d, DIR_INDEX_KEY)? {
                        if !in_range(c.key.offset) {
                            continue;
                        }
                        if let Some(l) = top.items.iter().find(|i| i.key == c.key) {
                            if l.data != c.data {
                                return Err(Error::DirtyLog);
                            }
                            continue;
                        }
                        let mut entries = dir::parse_dir_items(&c.data)?;
                        let (Some(entry), true) = (entries.pop(), entries.is_empty()) else {
                            return Err(Error::DirtyLog);
                        };
                        if entry.location_type != INODE_ITEM_KEY {
                            return Err(Error::DirtyLog);
                        }
                        let by_hash = key(d, dir::DIR_ITEM_KEY, dir::name_hash(&entry.name));
                        let hashed = tree.search(root, &by_hash)?.ok_or(Error::DirtyLog)?;
                        if hashed.data != c.data {
                            return Err(Error::DirtyLog);
                        }
                        edits.push(ItemEdit::Delete(c.key));
                        edits.push(ItemEdit::Delete(by_hash));
                        delta -= 2 * entry.name.len() as i64;
                        let child = entry.ino;
                        if logged_inodes.contains(&child) {
                            // A logged inode's names are its logged references.
                            let still = top.names_at(child, d, c.key.offset, &entry.name);
                            if still || !exists_only.contains(&child) {
                                return Err(Error::DirtyLog);
                            }
                            continue;
                        }
                        // An inode the log does not hold, losing this name: only
                        // its last, which deletes it with its data.
                        let refs = tree.find_all(root, child, INODE_REF_KEY)?;
                        let only = refs.len() == 1
                            && refs[0].key.offset == d
                            && ref_records(&refs[0].data).ok_or(Error::DirtyLog)?
                                == vec![(c.key.offset, &entry.name[..])];
                        let inode = tree
                            .search(root, &key(child, INODE_ITEM_KEY, 0))?
                            .ok_or(Error::DirtyLog)?;
                        let inode = Inode::parse(&inode.data, child)?;
                        if !only
                            || !inode.is_regular_file()
                            || inode.nlink != 1
                            || !removed_inodes.insert(child)
                        {
                            return Err(Error::DirtyLog);
                        }
                        let summed = inode.flags & INODE_NODATASUM == 0;
                        for kind in [
                            INODE_ITEM_KEY,
                            INODE_REF_KEY,
                            dir::XATTR_ITEM_KEY,
                            EXTENT_DATA_KEY,
                        ] {
                            for item in tree.find_all(root, child, kind)? {
                                if let Some(r) = (kind == EXTENT_DATA_KEY)
                                    .then(|| file_ref(item.key.offset, &item.data))
                                    .flatten()
                                {
                                    ledger.change(r, subvol, child, -1, summed, last_snapshot)?;
                                }
                                edits.push(ItemEdit::Delete(item.key));
                            }
                        }
                        for kind in [INODE_EXTREF_KEY, 48] {
                            // Extended references and orphan items are not
                            // deleted here.
                            if !tree.find_all(root, child, kind)?.is_empty() {
                                return Err(Error::DirtyLog);
                            }
                        }
                    }
                    // Logged names the directory lacks.
                    for item in top
                        .items
                        .iter()
                        .filter(|i| i.key.objectid == d && i.key.key_type == DIR_INDEX_KEY)
                    {
                        // A range says which committed names the log speaks
                        // for; a created directory has none.
                        if !created_dir && !in_range(item.key.offset) {
                            return Err(Error::DirtyLog);
                        }
                        if tree.search(root, &item.key)?.is_some() {
                            continue;
                        }
                        let entry = logged_entry(d, item.key.offset)?.ok_or(Error::DirtyLog)?;
                        // Its inode must be one the log holds and names here,
                        // in a reference or an extended reference (#407).
                        let named = top.names_at(entry.ino, d, item.key.offset, &entry.name);
                        if !named || entry.location_type != INODE_ITEM_KEY {
                            return Err(Error::DirtyLog);
                        }
                        let by_hash = key(d, dir::DIR_ITEM_KEY, dir::name_hash(&entry.name));
                        if tree.search(root, &by_hash)?.is_some() {
                            return Err(Error::DirtyLog);
                        }
                        edits.push(put(by_hash, item.data.clone()));
                        edits.push(put(item.key, item.data.clone()));
                        delta += 2 * entry.name.len() as i64;
                    }
                    let at = key(d, INODE_ITEM_KEY, 0);
                    let logged = top
                        .items
                        .iter()
                        .find(|i| i.key == at)
                        .ok_or(Error::DirtyLog)?;
                    let after = Inode::parse(&logged.data, d)?;
                    // A created directory starts empty and keeps its logged
                    // `nbytes`; a committed one keeps its committed count.
                    let (size, nbytes) = if created_dir {
                        (0, after.nbytes)
                    } else {
                        let c = tree.search(root, &at)?.ok_or(Error::DirtyLog)?;
                        let before = Inode::parse(&c.data, d)?;
                        if before.nlink != after.nlink
                            || before.generation != after.generation
                            || before.flags != after.flags
                        {
                            return Err(Error::DirtyLog);
                        }
                        (before.size, before.nbytes)
                    };
                    if logged.data.len() != INODE_ITEM_SIZE
                        || size.checked_add_signed(delta) != Some(after.size)
                        || grown.contains_key(&d)
                    {
                        return Err(Error::DirtyLog);
                    }
                    let mut data = logged.data.clone();
                    data[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
                    edits.push(put(at, data));
                }
                // A renamed inode must have lost every name its logged
                // references no longer hold.
                for &ino in &exists_only {
                    for c in tree.find_all(root, ino, INODE_REF_KEY)? {
                        let Some(l) = top.items.iter().find(|i| i.key == c.key) else {
                            continue;
                        };
                        let kept = ref_records(&l.data).ok_or(Error::DirtyLog)?;
                        for (index, name) in ref_records(&c.data).ok_or(Error::DirtyLog)? {
                            if kept.contains(&(index, name)) {
                                continue;
                            }
                            let gone = dirs.contains(&c.key.offset)
                                && top
                                    .items
                                    .iter()
                                    .filter(|i| {
                                        i.key.objectid == c.key.offset
                                            && i.key.key_type == DIR_LOG_INDEX_KEY
                                    })
                                    .any(|i| i.key.offset <= index && index <= le64(&i.data, 0))
                                && logged_entry(c.key.offset, index)?.is_none();
                            if !gone {
                                return Err(Error::DirtyLog);
                            }
                        }
                    }
                }
                // Each parent the log does not speak for, by its new names.
                for (parent, added) in grown {
                    let at = key(parent, INODE_ITEM_KEY, 0);
                    let mut raw = tree.search(root, &at)?.ok_or(Error::DirtyLog)?.data;
                    let size = le64(&raw, io::SIZE)
                        .checked_add_signed(added)
                        .ok_or(Error::DirtyLog)?;
                    raw[io::SIZE..io::SIZE + 8].copy_from_slice(&size.to_le_bytes());
                    edits.push(put(at, raw));
                }
                let mut seen = BTreeSet::new();
                if !edits.iter().all(|e| {
                    let k = e.key();
                    seen.insert((k.objectid, k.key_type, k.offset))
                }) || edits.is_empty()
                {
                    return Err(Error::DirtyLog);
                }

                if subvol == objectid::FS_TREE {
                    fs_edits.extend(edits);
                } else {
                    tree_edits.extend(edits.into_iter().map(|e| (subvol, e)));
                }
            }

            // Log blocks have no extent-tree allocation. Pin all of
            // them until the same commit both installs the replay and
            // clears the log, preserving recovery after a crash.
            let mut stack = vec![log.log_root];
            stack.extend(log.trees.iter().map(|t| t.root));
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

        // Each extent whose references replay changes: its record before
        // and after, the extents it allocates and those it releases.
        let mut records: Vec<RecordEdit> = Vec::new();
        let mut released: Vec<(u64, u64)> = Vec::new();
        let mut contents: Vec<(u64, Vec<u8>)> = Vec::new();
        {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let extent_root = self.tree_root(objectid::EXTENT_TREE)?;
            for (&(bytenr, len), ext) in &ledger.extents {
                if ext.refs.values().all(|&d| d == 0) {
                    continue;
                }
                for kind in [EXTENT_DATA_REF, SHARED_DATA_REF] {
                    if !tree.find_all(extent_root, bytenr, kind)?.is_empty() {
                        return Err(Error::DirtyLog);
                    }
                }
                let before = tree
                    .search(extent_root, &key(bytenr, EXTENT_ITEM, len))?
                    .map(|i| i.data);
                let mut body = match &before {
                    Some(b) => {
                        if b.len() < 24 || le64(b, 8) <= ext.last_snapshot {
                            return Err(Error::DirtyLog);
                        }
                        b.clone()
                    }
                    None => {
                        let mut b = Vec::with_capacity(24);
                        for v in [0, generation, EXTENT_FLAG_DATA] {
                            b.extend_from_slice(&v.to_le_bytes());
                        }
                        b
                    }
                };
                let total = le64(&body, 0) as i64 + ext.refs.values().sum::<i64>();
                if total < 0 {
                    return Err(Error::DirtyLog);
                }
                let after = if total == 0 {
                    // Released: every reference it had was one replay drops.
                    let refs = backref::inline_refs(EXTENT_ITEM, &body)?;
                    for r in &refs {
                        let fields = |at: usize| le64(&r.payload, at);
                        let ours = r.kind == EXTENT_DATA_REF
                            && ext
                                .refs
                                .get(&(fields(0), fields(8), fields(16)))
                                .copied()
                                .unwrap_or(0)
                                == -i64::from(u32::from_le_bytes(
                                    r.payload[24..28].try_into().expect("28-byte payload"),
                                ));
                        if !ours {
                            return Err(Error::DirtyLog);
                        }
                    }
                    released.push((bytenr, bytenr + len));
                    None
                } else {
                    // Every reference added before any is dropped, so a drop
                    // never meets an extent's last reference.
                    for (&(tree_id, ino, offset), &d) in &ext.refs {
                        for _ in 0..d.max(0) {
                            body = backref::add_inline_data_ref(
                                EXTENT_ITEM,
                                &body,
                                tree_id,
                                ino,
                                offset,
                            )?;
                        }
                    }
                    for (&(tree_id, ino, offset), &d) in &ext.refs {
                        for _ in 0..(-d).max(0) {
                            body = backref::drop_inline_data_ref(
                                EXTENT_ITEM,
                                &body,
                                tree_id,
                                ino,
                                offset,
                            )?
                            .ok_or(Error::DirtyLog)?;
                        }
                    }
                    Some(body)
                };
                if before.is_none() {
                    let mut buf = vec![0u8; usize::try_from(len).map_err(|_| Error::DirtyLog)?];
                    Self::read_logical_pool(
                        &self.device,
                        &self.devices,
                        &self.map,
                        bytenr,
                        &mut buf,
                    )?;
                    if ext.summed {
                        contents.push((bytenr, buf));
                    }
                }
                records.push(RecordEdit {
                    bytenr,
                    len,
                    before,
                    after,
                });
            }
        }

        // The new extents' digests, from the bytes already on disk, and
        // the released extents' removed. The log carries the digests the
        // kernel computed; they must be exactly these, sector for sector,
        // or the data did not reach the disk as logged.
        let csum_edits = if records.is_empty() {
            Vec::new()
        } else {
            self.csum_edits(&released, &contents, true)?
        };
        let size = self.sb.csum_type.digest_len();
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
                edits: fs_edits,
                tree_edits,
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

    /// The key offsets a Debian 12 (6.1) kernel and a 7.0 kernel both gave
    /// the extended references of names 061 to 064 (each three digits
    /// repeated to 255 bytes) under `/`, read with the native
    /// `dump-tree` (#407).
    #[test]
    fn an_extended_reference_is_filed_under_the_kernels_hash() {
        for (i, hash) in [
            (61, 1_271_905_755),
            (62, 992_780_333),
            (63, 3_895_035_600),
            (64, 3_672_808_385),
        ] {
            let name = format!("{i:03}").repeat(85);
            assert_eq!(extref_hash(256, name.as_bytes()), hash, "name {i}");
            let mut data = 256u64.to_le_bytes().to_vec();
            data.extend((i as u64 + 3).to_le_bytes());
            data.extend(255u16.to_le_bytes());
            data.extend(name.as_bytes());
            let at = |offset| DiskKey {
                objectid: 257,
                key_type: INODE_EXTREF_KEY,
                offset,
            };
            assert_eq!(
                name_records(&at(hash), &data),
                Some(vec![(256, i as u64 + 3, name.as_bytes())]),
                "name {i}"
            );
            assert_eq!(name_records(&at(hash + 1), &data), None, "name {i}");
            assert_eq!(name_records(&at(hash), &data[..data.len() - 1]), None);
            let tree = LoggedTree {
                subvolume: 5,
                root: 0,
                items: vec![item(257, INODE_EXTREF_KEY, hash, data)],
            };
            assert_eq!(tree.inode_named(256, name.as_bytes()), Some(257));
            assert!(tree.names_at(257, 256, i as u64 + 3, name.as_bytes()));
            assert!(!tree.names_at(257, 257, i as u64 + 3, name.as_bytes()));
        }
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

    /// A regular extent item: address, length on disk, offset into it,
    /// and the bytes of file it covers.
    fn regular(bytenr: u64, disk_len: u64, offset: u64, num: u64) -> Vec<u8> {
        let mut d = vec![0u8; fe::REGULAR_SIZE];
        d[fe::RAM_BYTES..fe::RAM_BYTES + 8].copy_from_slice(&disk_len.to_le_bytes());
        d[fe::TYPE] = 1;
        for (at, v) in [
            (fe::DISK_BYTENR, bytenr),
            (fe::DISK_NUM_BYTES, disk_len),
            (fe::OFFSET, offset),
            (fe::NUM_BYTES, num),
        ] {
            d[at..at + 8].copy_from_slice(&v.to_le_bytes());
        }
        d
    }

    /// The shapes of the data-and-names fixture (#375): a write inside a
    /// committed extent keeps its head and tail as references into it, an
    /// append adds an item, and a truncate replaces the committed item and
    /// cuts what lies past the logged size.
    #[test]
    fn logged_items_replace_what_they_cover_and_the_file_is_cut_at_its_size() {
        let k = 4096;
        let committed = BTreeMap::from([(0, regular(1 << 20, 64 * k, 0, 64 * k))]);
        let overwrite = BTreeMap::from([(16 * k, regular(2 << 20, k, 0, k))]);
        let after = reconcile(&committed, &overwrite, 64 * k).unwrap();
        assert_eq!(
            after,
            BTreeMap::from([
                (0, regular(1 << 20, 64 * k, 0, 16 * k)),
                (16 * k, regular(2 << 20, k, 0, k)),
                (17 * k, regular(1 << 20, 64 * k, 17 * k, 47 * k)),
            ])
        );
        // Both remaining pieces name the committed extent from the same
        // file offset, so they are two of one reference.
        assert_eq!(file_ref(0, &after[&0]), Some((1 << 20, 64 * k, 0)));
        assert_eq!(
            file_ref(17 * k, &after[&(17 * k)]),
            Some((1 << 20, 64 * k, 0))
        );

        let append = BTreeMap::from([(64 * k, regular(3 << 20, 2 * k, 0, 2 * k))]);
        let after = reconcile(&committed, &append, 66 * k).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(
            after.values().map(|d| counted_bytes(d)).sum::<u64>(),
            66 * k
        );

        let truncate = BTreeMap::from([(0, regular(1 << 20, 64 * k, 0, 3 * k))]);
        let after = reconcile(&committed, &truncate, 3 * k).unwrap();
        assert_eq!(after, truncate);

        let mut compressed = regular(1 << 20, 64 * k, 0, 64 * k);
        compressed[fe::COMPRESSION] = 1;
        assert!(
            reconcile(&BTreeMap::from([(0, compressed)]), &overwrite, 64 * k).is_err(),
            "a window into part of a compressed extent"
        );
        let beside = BTreeMap::from([(8 * k, regular(2 << 20, k, 0, k))]);
        assert!(
            reconcile(&BTreeMap::from([(0, inline(b"inline"))]), &beside, 9 * k).is_err(),
            "an inline extent left beside another item"
        );
    }

    /// Replay takes an inline extent of any length, and a compressed one
    /// as logged (#378); encryption, other encodings, an unknown
    /// compression, a regular extent and a plain one whose `ram_bytes`
    /// disagrees with its data stay refused.
    #[test]
    fn an_inline_extent_of_any_length_or_compression_is_replayable() {
        assert!(inline_extent(&inline(b"committed")));
        assert!(inline_extent(&inline(b"a longer rewrite of it")));
        for algo in 1..=3 {
            let mut compressed = inline(b"zzzz");
            compressed[fe::COMPRESSION] = algo;
            compressed[fe::RAM_BYTES] = 33;
            assert!(inline_extent(&compressed), "compression {algo}");
        }
        let mut unknown = inline(b"zzzz");
        unknown[fe::COMPRESSION] = 4;
        assert!(!inline_extent(&unknown), "unknown compression");
        for at in [fe::ENCRYPTION, fe::OTHER_ENCODING, fe::OTHER_ENCODING + 1] {
            let mut encoded = inline(b"rewritten");
            encoded[at] = 1;
            assert!(!inline_extent(&encoded), "encoding byte {at}");
        }
        let mut external = inline(b"rewritten");
        external[fe::TYPE] = 1;
        assert!(!inline_extent(&external), "external extent");
        let mut ram = inline(b"rewritten");
        ram[fe::RAM_BYTES] = 3;
        assert!(!inline_extent(&ram), "plain ram_bytes disagrees with data");
        assert!(!inline_extent(&inline(b"")), "no inline data");
    }
}
