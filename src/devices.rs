//! The devices a filesystem spans: adding one (#303).
//!
//! # Adding a device
//!
//! A device joins a filesystem in one committed transaction, which says
//! so in three places:
//!
//! - a `DEV_ITEM` for it in the **chunk tree**, keyed `(DEV_ITEMS,
//!   DEV_ITEM, devid)`: the devid after the highest the chunk tree holds,
//!   a fresh device UUID, the device's size rounded down to a whole
//!   sector, nothing used, and the filesystem's UUID (its metadata UUID,
//!   when it has one) as every other member's item carries it;
//! - **every member's superblock**: `num_devices` one more and
//!   `total_bytes` grown by the new device's size, each member keeping its
//!   own `dev_item`;
//! - **the new device's superblock**, the pool's with the new device item
//!   as its `dev_item`, at every copy offset the device is large enough to
//!   hold.
//!
//! The chunk tree is copy-on-write like every other tree and its root is
//! named by the superblock, so the commit moves `chunk_root` as a resize
//! does. No chunk is allocated on the new device and the device tree does
//! not change: a `DEV_EXTENT` exists only for a chunk's stripe, and the
//! kernel allocates onto the new device as it needs the space, exactly as
//! after its own `btrfs device add`.
//!
//! # The order of the superblocks
//!
//! The new device's superblocks are written before the members'. A commit
//! cut between the two then leaves one of two whole filesystems: the
//! members at the old generation, one device fewer, never having heard of
//! the new one; or, read through the new device's superblock, the new
//! generation, whose tree blocks were all flushed to the members before
//! any superblock was written. The other order can leave members that
//! count a device which carries no superblock at all, a pool that only
//! mounts degraded.
//!
//! # What it refuses
//!
//! - a device already carrying a superblock of this filesystem: it is a
//!   member, or was one, and adding it twice would give one disk two
//!   devids;
//! - a device no larger than the first MiB, which every Btrfs device keeps
//!   to itself and places no chunk in;
//! - a mount that cannot write, or a device that cannot be written.
//!
//! A device holding some other filesystem is not refused: it is the
//! caller's to decide, as it is for [`crate::mkfs::format`], and
//! [`crate::mkfs::existing_signature`] says what is there.

use std::sync::Arc;

use fs_core::BlockDevice;

use crate::chunk::{key_type, objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::superblock::{dev_item_offsets as o, DEV_ITEM_SIZE};
use crate::transaction::{DataWrite, ItemEdit};

/// How many rounds the plan has to close over its own bookkeeping.
const PLAN_ROUNDS: usize = 64;

/// The first MiB of every device, which Btrfs keeps for itself: the
/// primary superblock is in it, and no device extent starts below it.
pub const RESERVED_BYTES: u64 = 1 << 20;

/// What a new device's item says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NewDevice {
    /// The id chunk stripes will name it by.
    pub devid: u64,
    /// Its size, a whole number of sectors.
    pub total_bytes: u64,
    /// The filesystem's sector size, its I/O alignment, width and sector.
    pub sectorsize: u32,
    /// Its own UUID.
    pub uuid: [u8; 16],
    /// The filesystem's, as every member's item records it.
    pub fsid: [u8; 16],
}

impl NewDevice {
    /// The device item, as the chunk tree and the device's superblock hold
    /// it: nothing used, and every hint zero.
    pub(crate) fn item(&self) -> Vec<u8> {
        let mut out = vec![0u8; DEV_ITEM_SIZE];
        let mut put64 = |at: usize, v: u64| out[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put64(o::DEVID, self.devid);
        put64(o::TOTAL_BYTES, self.total_bytes);
        let mut put32 = |at: usize, v: u32| out[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put32(o::IO_ALIGN, self.sectorsize);
        put32(o::IO_WIDTH, self.sectorsize);
        put32(o::SECTOR_SIZE, self.sectorsize);
        out[o::UUID..o::UUID + 16].copy_from_slice(&self.uuid);
        out[o::FSID..o::FSID + 16].copy_from_slice(&self.fsid);
        out
    }
}

impl Filesystem {
    /// Add `device` to the filesystem, as one committed transaction, and
    /// return the devid it was given. The mount becomes the pool's: every
    /// member and the new device, all of them written from here on.
    ///
    /// The device's previous contents are not read and, past its
    /// superblocks, not cleared: nothing in the filesystem points at them
    /// until the kernel allocates a chunk there and writes it.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless the mount can write and so can the
    /// device, and [`Error::UnsupportedFeature`] for every refusal the
    /// [module documentation](crate::devices) lists. Nothing is written
    /// unless everything is.
    pub fn add_device(&mut self, device: Arc<dyn BlockDevice>) -> Result<u64> {
        if self.writable.is_none() || !device.is_writable() {
            return Err(Error::ReadOnly);
        }
        let sectorsize = u64::from(self.sb.sectorsize);
        let size = device.size_bytes() - device.size_bytes() % sectorsize;
        if size <= RESERVED_BYTES {
            return Err(Error::UnsupportedFeature(format!(
                "the device holds {size} bytes, and a Btrfs device keeps its first \
                 {RESERVED_BYTES} to itself: there is no room on it for a chunk"
            )));
        }
        if let Ok((sb, _)) = crate::superblock::read_superblock(&*device) {
            if sb.fsid == self.sb.fsid {
                return Err(Error::UnsupportedFeature(format!(
                    "the device already carries a superblock of this filesystem, as device \
                     {}: it is a member, or was one, and must be wiped before it is added",
                    sb.dev_item.devid
                )));
            }
        }

        let last = self.last_devid()?;
        let devid = last.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature(format!("device {last} is the last devid there is"))
        })?;
        let item = NewDevice {
            devid,
            total_bytes: size,
            sectorsize: self.sb.sectorsize,
            uuid: crate::mkfs::random_uuid(),
            fsid: self.sb.dev_item.fsid,
        }
        .item();

        let key = DiskKey {
            objectid: objectid::DEV_ITEMS,
            key_type: key_type::DEV_ITEM,
            offset: devid,
        };
        let leaf = {
            let reader = self.pool_reader();
            let tree = reader.tree();
            tree.descend(self.sb.chunk_root, &key)?.header.bytenr
        };
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let write = DataWrite {
            root: objectid::CHUNK_TREE,
            edits: vec![ItemEdit::Put(OwnedItem {
                key,
                data: item.clone(),
            })],
            ..Default::default()
        };
        let plan = self.plan_transaction_closed_with(&[leaf], &write, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, &write, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("adding a device's plan does not move the root tree".into())
        })?;
        let chunk_root = self.planned_chunk_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("adding a device's plan does not move the chunk tree".into())
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
        self.commit_adding_device(
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
            &device,
            &item,
        )?;
        let mut members = self.write_devices();
        members.push(device);
        *self = Filesystem::mount_pool_rw(members)?;
        Ok(devid)
    }

    /// The highest devid the chunk tree has a `DEV_ITEM` for.
    fn last_devid(&self) -> Result<u64> {
        let reader = self.pool_reader();
        let tree = reader.tree();
        let mut last = None;
        tree.for_each(self.sb.chunk_root, &mut |k: &DiskKey, _: &[u8]| {
            if k.objectid == objectid::DEV_ITEMS && k.key_type == key_type::DEV_ITEM {
                last = last.max(Some(k.offset));
            }
            Ok(true)
        })?;
        last.ok_or_else(|| {
            Error::UnsupportedFeature("the chunk tree holds no device item at all".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superblock::DevItem;

    /// The item reads back, through the parser the mount uses, as the
    /// device it was made for: every field where `btrfs_dev_item` has it.
    #[test]
    fn a_new_device_item_reads_back_as_the_device() {
        let new = NewDevice {
            devid: 3,
            total_bytes: 256 << 20,
            sectorsize: 4096,
            uuid: [0x11; 16],
            fsid: [0x22; 16],
        };
        let item = new.item();
        assert_eq!(item.len(), DEV_ITEM_SIZE);
        let parsed = DevItem::parse(&item).expect("a device item");
        assert_eq!(parsed.devid, 3);
        assert_eq!(parsed.total_bytes, 256 << 20);
        assert_eq!(parsed.bytes_used, 0);
        assert_eq!(
            (parsed.io_align, parsed.io_width, parsed.sector_size),
            (4096, 4096, 4096)
        );
        assert_eq!(
            (
                parsed.dev_type,
                parsed.generation,
                parsed.start_offset,
                parsed.dev_group
            ),
            (0, 0, 0, 0)
        );
        assert_eq!((parsed.seek_speed, parsed.bandwidth), (0, 0));
        assert_eq!(parsed.uuid, [0x11; 16]);
        assert_eq!(parsed.fsid, [0x22; 16]);
    }
}
