//! Changing a filesystem's size (#264).
//!
//! A single-device filesystem's size is said in three places, and a
//! resize is one committed transaction that changes all three:
//!
//! - the device's `DEV_ITEM` in the **chunk tree**, keyed
//!   `(DEV_ITEMS, DEV_ITEM, devid)`, whose `total_bytes` is what the
//!   kernel allocates chunks against;
//! - the superblock's `total_bytes`;
//! - the copy of the device item embedded in the superblock.
//!
//! The chunk tree is copy-on-write like every other tree, but its blocks
//! live in a SYSTEM block group and its root is named by the superblock
//! rather than by a `ROOT_ITEM`, so the commit moves the superblock's
//! `chunk_root` too. No chunk is allocated: the kernel allocates into new
//! space as it needs it, exactly as after its own `btrfs filesystem
//! resize`.
//!
//! # What it refuses
//!
//! - **growing past the device**: the caller makes the device (or the
//!   image file) larger first;
//! - **shrinking past a device extent**: a chunk that lies beyond the new
//!   end would have to be relocated first, which is a balance, not a
//!   resize;
//! - a filesystem of more than one device.

use crate::chunk::{key_type, objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::transaction::{DataWrite, ItemEdit};

/// How many rounds a resize's plan has to close over its own bookkeeping.
const PLAN_ROUNDS: usize = 64;

/// `BTRFS_DEV_EXTENT_KEY`.
const DEV_EXTENT_KEY: u8 = 204;

/// Where a `DEV_ITEM`'s `total_bytes` is, and a `DEV_EXTENT`'s `length`.
const DEV_ITEM_TOTAL_BYTES: usize = 8;
const DEV_EXTENT_LENGTH: usize = 24;

impl Filesystem {
    /// Make the filesystem `size` bytes, rounded down to a whole sector, as
    /// one committed transaction. The device must already hold that many
    /// bytes; after a shrink, the caller may cut the device to the new
    /// size.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// and [`Error::UnsupportedFeature`] for every refusal the [module
    /// documentation](crate::resize) lists. Nothing is written unless
    /// everything is.
    pub fn resize(&mut self, size: u64) -> Result<()> {
        let Some(device) = self.writable.clone() else {
            return Err(Error::ReadOnly);
        };
        if self.sb.num_devices != 1 {
            return Err(Error::UnsupportedFeature(format!(
                "this filesystem spans {} devices, and resizing a pool is not implemented",
                self.sb.num_devices
            )));
        }
        let size = size - size % u64::from(self.sb.sectorsize);
        if size == self.sb.total_bytes {
            return Ok(());
        }
        let device_len = device.size_bytes();
        if size > device_len {
            return Err(Error::UnsupportedFeature(format!(
                "the device holds {device_len} bytes, fewer than {size}: make it larger first"
            )));
        }
        let devid = self.sb.dev_item.devid;
        let in_use = self.device_extents_end(devid)?;
        if size < in_use {
            return Err(Error::UnsupportedFeature(format!(
                "device {devid} has a chunk reaching {in_use}, past {size}: moving it is a \
                 balance, which is not implemented"
            )));
        }

        // The device item, with its new size.
        let key = DiskKey {
            objectid: objectid::DEV_ITEMS,
            key_type: key_type::DEV_ITEM,
            offset: devid,
        };
        let (mut item, leaf) = {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let item = tree
                .search(self.sb.chunk_root, &key)?
                .ok_or_else(|| {
                    Error::UnsupportedFeature(format!(
                        "the chunk tree has no item for device {devid}"
                    ))
                })?
                .data;
            let leaf = tree.descend(self.sb.chunk_root, &key)?.header.bytenr;
            (item, leaf)
        };
        if item.len() < DEV_ITEM_TOTAL_BYTES + 8 {
            return Err(Error::UnsupportedFeature(format!(
                "device {devid}'s item is {} bytes, shorter than a device item",
                item.len()
            )));
        }
        item[DEV_ITEM_TOTAL_BYTES..DEV_ITEM_TOTAL_BYTES + 8].copy_from_slice(&size.to_le_bytes());

        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let write = DataWrite {
            root: objectid::CHUNK_TREE,
            edits: vec![ItemEdit::Put(OwnedItem { key, data: item })],
            ..Default::default()
        };
        let plan = self.plan_transaction_closed_with(&[leaf], &write, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, &write, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the resize's plan does not move the root tree".into())
        })?;
        let chunk_root = self.planned_chunk_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the resize's plan does not move the chunk tree".into())
        })?;
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
        self.commit_sized(
            &blocks,
            &Commit {
                generation,
                root,
                bytes_used,
                chunk_root: Some(chunk_root),
                chunk_root_generation: Some(generation),
                invalidate_free_space_tree: false,
                ..Default::default()
            },
            Some(size),
        )?;
        *self = Filesystem::mount_rw(device)?;
        Ok(())
    }

    /// Where the last chunk on device `devid` ends: the greatest
    /// `physical + length` of its `DEV_EXTENT` items in the device tree.
    fn device_extents_end(&self, devid: u64) -> Result<u64> {
        let root = self.tree_root(objectid::DEV_TREE)?;
        let reader = self.pool_reader();
        let tree = reader.tree();
        let mut end = 0u64;
        let mut bad = None;
        tree.for_each(root, &mut |k: &DiskKey, data: &[u8]| {
            if k.objectid == devid && k.key_type == DEV_EXTENT_KEY {
                match data.get(DEV_EXTENT_LENGTH..DEV_EXTENT_LENGTH + 8) {
                    Some(b) => {
                        let len = u64::from_le_bytes(b.try_into().expect("8 bytes"));
                        end = end.max(k.offset.saturating_add(len));
                    }
                    None => bad = Some(k.offset),
                }
            }
            Ok(true)
        })?;
        if let Some(at) = bad {
            return Err(Error::UnsupportedFeature(format!(
                "device {devid}'s extent at {at} is too short to say its length"
            )));
        }
        Ok(end)
    }
}
