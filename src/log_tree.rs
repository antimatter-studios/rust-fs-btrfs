//! Reading a log tree the kernel has not replayed yet (#266, first slice).
//!
//! An `fsync` does not commit a transaction. It writes what the synced
//! inodes now hold into a **log tree** per subvolume, names those trees
//! from a **log root tree**, and points the superblock's `log_root` at
//! that. The next mount replays the log into the real trees. Until then
//! the committed trees hold the state before the `fsync`, which is why
//! every mount here refuses such a volume with [`Error::DirtyLog`]:
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
//! # What it does not do
//!
//! Replay. Folding the log into the trees is the rest of #266; until it
//! lands, a writable mount still refuses a non-empty log.
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
    pub const COMPRESSION: usize = 16;
    pub const ENCRYPTION: usize = 17;
    pub const TYPE: usize = 20;
    pub const INLINE_DATA: usize = 21;
    pub const DISK_BYTENR: usize = 21;
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

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

impl Filesystem {
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
}
