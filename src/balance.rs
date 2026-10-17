//! Moving a block group's contents somewhere else: balance (#304).
//!
//! Relocating a block group starts from knowing what is in it: every data
//! extent, and every `EXTENT_DATA` item that has to be pointed somewhere
//! else once the extent moves (#396).

use crate::error::{Error, Result};
use crate::fs::Filesystem;

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
    /// How far into the extent its first byte is.
    pub extent_offset: u64,
    /// How many bytes of the file it covers.
    pub len: u64,
}

impl Filesystem {
    /// Every data extent of the data block group at `start`, in address
    /// order, each with every file item naming it.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`]: not implemented yet.
    pub fn block_group_extents(&self, start: u64) -> Result<Vec<GroupExtent>> {
        Err(Error::UnsupportedFeature(format!(
            "enumerating the extents of the block group at {start} is not implemented"
        )))
    }
}
