//! Overwriting file data in place.
//!
//! Btrfs is copy-on-write, so as a rule nothing is written where it
//! already is: a change allocates a new block, records a new checksum,
//! rewrites the B-tree path to the root and commits a new superblock
//! generation. None of that is possible without a transaction engine,
//! and a partial attempt at one produces a filesystem its own checker
//! rejects.
//!
//! There is one exception, and it is the whole of what this module does.
//!
//! # `nodatacow`
//!
//! A file marked `NODATACOW` is deliberately exempt from copy-on-write:
//! its data blocks are overwritten where they lie. Btrfs also clears
//! checksumming for such a file — the flag is set together with
//! `NODATASUM`, because a block that changes in place cannot keep a
//! checksum that was computed elsewhere and committed separately.
//!
//! So for these files, and only these, writing the bytes changes nothing
//! else. No block is allocated. No checksum item exists to update. No
//! tree node is rewritten and no generation is committed. The write is
//! exactly as safe as the equivalent on a filesystem that never had
//! copy-on-write to begin with.
//!
//! # The one thing that is not obvious
//!
//! `NODATACOW` is not a promise that the extent will be written in
//! place. It is a promise that it will be written in place **while it
//! belongs to one file**. Take a snapshot and the extent becomes shared;
//! the next write to it copies first, because the snapshot must keep
//! seeing what it saw. A driver that honoured the flag and skipped that
//! check would silently rewrite what a snapshot is still pointing at.
//!
//! Sharing is visible in the extent tree, as a reference count on the
//! extent item. This reads it, and refuses anything above one. That
//! lookup is the reason this module is more than a byte copy.

use crate::btree::Tree;
use crate::chunk::DiskKey;
use crate::error::{Error, Result};
use crate::fs::{Filesystem, ROOT_ITEM_KEY};

/// `BTRFS_INODE_NODATASUM` — this file's blocks carry no checksums.
pub const INODE_NODATASUM: u64 = 1 << 0;
/// `BTRFS_INODE_NODATACOW` — this file's blocks are written in place.
pub const INODE_NODATACOW: u64 = 1 << 1;

/// `BTRFS_EXTENT_TREE_OBJECTID` — the tree holding reference counts.
const EXTENT_TREE_OBJECTID: u64 = 2;
/// `BTRFS_EXTENT_ITEM_KEY`.
const EXTENT_ITEM_KEY: u8 = 168;

/// Offsets within `btrfs_extent_item`.
mod extent_item {
    /// How many references point at this extent. One means it belongs
    /// to a single file and nothing else is looking at it.
    pub const REFS: usize = 0;
}

impl Filesystem {
    /// Overwrite `data` at `offset` in a `nodatacow` file.
    ///
    /// Returns the number of bytes written, always `data.len()` on
    /// success. Nothing is written unless the whole range can be, so a
    /// range that turns out to be unwritable partway through leaves the
    /// file untouched rather than half updated.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// and [`Error::UnsupportedFeature`] naming which condition failed
    /// for everything else — a caller deciding whether to fall back
    /// needs to know whether it met a shared extent or a compressed one,
    /// not merely that the write was declined.
    pub fn write_at(&self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        if data.is_empty() {
            return Ok(0);
        }

        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() {
            return Err(Error::NotAFile);
        }
        if inode.flags & INODE_NODATACOW == 0 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} is copy-on-write, so overwriting it in place would leave the \
                 extent tree, the checksum tree and the superblock generation describing \
                 something that is no longer there"
            )));
        }
        if inode.flags & INODE_NODATASUM == 0 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} is nodatacow but still checksummed, so writing it in place \
                 would leave every checksum item for it wrong"
            )));
        }

        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::UnsupportedFeature("write range overflows".into()))?;
        if end > inode.size {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: writing to {end} would grow the file past its {} bytes, \
                 which allocates",
                inode.size
            )));
        }

        // Resolve everything before writing anything.
        let plan = self.plan_nodatacow_write(ino, offset, data.len())?;

        let device = self.writable.as_ref().expect("checked above");
        let mut done = 0usize;
        for (logical, len) in plan {
            Self::write_logical(device, &self.map, logical, &data[done..done + len])?;
            done += len;
        }
        device.flush()?;
        Ok(done)
    }

    /// Whether `ino` can be written in place at all.
    ///
    /// Answers the question a caller actually has before offering a file
    /// as editable, rather than making them attempt a write and read the
    /// refusal. It applies the same conditions the write does, to the
    /// whole file rather than to one range — a file is reported writable
    /// only if every extent of it could be overwritten.
    ///
    /// A file with no extents at all — empty, or entirely holes — is
    /// reported writable, since there is nothing there that would have
    /// to be refused. Any write to it would still be refused for
    /// exceeding its size, which is the correct answer for a different
    /// reason.
    pub fn can_write_in_place(&self, ino: u64) -> Result<bool> {
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() {
            return Ok(false);
        }
        if inode.flags & (INODE_NODATACOW | INODE_NODATASUM) != (INODE_NODATACOW | INODE_NODATASUM)
        {
            return Ok(false);
        }
        for piece in self.file_extents(ino)? {
            if piece.compressed || piece.logical.is_none() {
                return Ok(false);
            }
            if self.extent_refs(piece.extent_start)? != 1 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Where each part of the write lands, as (logical address, length).
    ///
    /// Every refusal happens here, while the file is still untouched.
    fn plan_nodatacow_write(&self, ino: u64, offset: u64, len: usize) -> Result<Vec<(u64, usize)>> {
        let pieces = self.file_extents(ino)?;
        let mut plan = Vec::new();
        let mut done = 0usize;

        while done < len {
            let pos = offset + done as u64;
            let Some(piece) = pieces
                .iter()
                .find(|p| pos >= p.start && pos < p.start + p.len)
            else {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: offset {pos} is a hole, and filling it would allocate"
                )));
            };
            if piece.compressed {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: offset {pos} is in a compressed extent, which cannot be \
                     rewritten without recompressing the whole of it"
                )));
            }
            let Some(logical) = piece.logical else {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: offset {pos} is inline or preallocated, so writing it \
                     changes the item rather than the blocks it points at"
                )));
            };

            // The check that `nodatacow` alone does not give us.
            let refs = self.extent_refs(piece.extent_start)?;
            if refs != 1 {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: the extent at {} has {refs} references, so something \
                     else — most likely a snapshot — is still reading it",
                    piece.extent_start
                )));
            }

            let within = pos - piece.start;
            let chunk = ((piece.len - within) as usize).min(len - done);
            plan.push((logical + within, chunk));
            done += chunk;
        }
        Ok(plan)
    }

    /// How many references the extent beginning at `bytenr` has.
    ///
    /// One means it belongs to a single file. Anything more means a
    /// snapshot or a reflink is also pointing at it, and writing in
    /// place would change what that other reader sees.
    fn extent_refs(&self, bytenr: u64) -> Result<u64> {
        let root = self.extent_tree_root()?;
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical_pool(&self.device, &self.devices, &self.map, logical, buf)
        };
        let tree = Tree::from_superblock(&self.sb, &read);

        let mut refs = None;
        tree.for_each(root, &mut |key: &DiskKey, data: &[u8]| {
            if key.objectid == bytenr
                && key.key_type == EXTENT_ITEM_KEY
                && data.len() >= extent_item::REFS + 8
            {
                refs = Some(u64::from_le_bytes(
                    data[extent_item::REFS..extent_item::REFS + 8]
                        .try_into()
                        .expect("8 bytes"),
                ));
                return Ok(false);
            }
            Ok(true)
        })?;

        // An extent with no item is not "unreferenced" — it is an extent
        // this driver failed to find, and treating the two the same
        // would turn a lookup bug into a write over shared data.
        refs.ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "the extent tree holds no item for the extent at {bytenr}, so whether it \
                 is shared cannot be established"
            ))
        })
    }

    /// The extent tree's root, named by the root tree.
    fn extent_tree_root(&self) -> Result<u64> {
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical_pool(&self.device, &self.devices, &self.map, logical, buf)
        };
        let tree = Tree::from_superblock(&self.sb, &read);
        let mut root = None;
        tree.for_each(self.sb.root, &mut |key: &DiskKey, data: &[u8]| {
            if key.objectid == EXTENT_TREE_OBJECTID
                && key.key_type == ROOT_ITEM_KEY
                && data.len() > crate::fs::root_item::LEVEL
            {
                root = Some(u64::from_le_bytes(
                    data[crate::fs::root_item::BYTENR..crate::fs::root_item::BYTENR + 8]
                        .try_into()
                        .expect("8 bytes"),
                ));
                return Ok(false);
            }
            Ok(true)
        })?;
        root.ok_or_else(|| {
            Error::BadSuperblock("the root tree holds no ROOT_ITEM for the extent tree".into())
        })
    }
}
