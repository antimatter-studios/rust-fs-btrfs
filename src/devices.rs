//! The devices a filesystem spans: adding one, and replacing one (#303).
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
//!
//! # Replacing a device
//!
//! A replaced device's place is taken whole: the new device gets every
//! byte of every `DEV_EXTENT` the old one holds, at the same physical
//! offset, and then the old device's superblock, so it becomes the same
//! devid with the same device UUID and the same recorded size. That is
//! where the kernel's own `btrfs replace` leaves a pool, and it means
//! nothing else in the filesystem changes: every chunk stripe names the
//! device by devid and UUID, and every device extent by its offset, and
//! all of those still hold. No tree is rewritten and the generation does
//! not move. A larger new device keeps the old one's size until it is
//! resized, as after the kernel's replace.
//!
//! The order is what makes it safe to cut anywhere:
//!
//! 1. every device extent copied, and the new device flushed: until its
//!    superblock is written, nothing reads the new device as a member;
//! 2. the new device's superblocks, and a flush: from here the new device
//!    is the member;
//! 3. the old device's superblocks wiped, and a flush, so the old device is
//!    no longer found as one.
//!
//! Cut between 2 and 3, both devices claim the same devid and hold the same
//! bytes in every device extent; either one completes the pool, and the old
//! device should be wiped before both are attached together.
//!
//! A replace refuses a device smaller than the one it replaces, a device
//! already carrying a superblock of this filesystem, a devid that is not a
//! member, and a mount or a device that cannot write. The old device must
//! be readable: rebuilding a missing or failed device from its mirrors is
//! not implemented.

use std::sync::Arc;

use fs_core::BlockDevice;

use crate::chunk::{key_type, objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::super_write::{self, SUPERBLOCK_SIZE};
use crate::superblock::{dev_item_offsets as o, DEV_ITEM_SIZE, SUPER_OFFSETS};
use crate::transaction::{DataWrite, ItemEdit};

/// How many rounds the plan has to close over its own bookkeeping.
const PLAN_ROUNDS: usize = 64;

/// The first MiB of every device, which Btrfs keeps for itself: the
/// primary superblock is in it, and no device extent starts below it.
pub const RESERVED_BYTES: u64 = 1 << 20;

/// `BTRFS_DEV_EXTENT_KEY`, the device tree's item for one chunk stripe.
const DEV_EXTENT_KEY: u8 = 204;

/// Where a `DEV_EXTENT`'s `length` is.
const DEV_EXTENT_LENGTH: usize = 24;

/// How much of a device extent a replace copies at a time.
const COPY_BYTES: usize = 1 << 20;

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
        self.refuse_member(&device, "added")?;

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

    /// Replace device `devid` with `target`, which takes its place whole:
    /// its devid, its device UUID, its recorded size and every byte of
    /// every device extent on it. The old device's superblocks are wiped,
    /// and the mount becomes the pool's with `target` in the old device's
    /// place.
    ///
    /// Nothing but the two devices is written, and nothing the pool reads
    /// until every device extent is copied; the [module
    /// documentation](crate::devices) gives the order and what a cut
    /// leaves.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless the mount can write and so can `target`,
    /// and [`Error::UnsupportedFeature`] for every refusal the module
    /// documentation lists, each before anything is written.
    pub fn replace_device(&mut self, devid: u64, target: Arc<dyn BlockDevice>) -> Result<()> {
        if self.writable.is_none() || !target.is_writable() {
            return Err(Error::ReadOnly);
        }
        let source = if self.writable_pool.is_empty() {
            self.writable
                .clone()
                .filter(|_| devid == self.sb.dev_item.devid)
        } else {
            self.writable_pool.get(&devid).cloned()
        }
        .ok_or_else(|| {
            Error::UnsupportedFeature(format!("device {devid} is not a member of this filesystem"))
        })?;
        self.refuse_member(&target, "the replacement")?;

        // The old device's own superblock, which the new device is given
        // as it stands: it names the devid, the device UUID and the size.
        let (sb, copy) = crate::superblock::read_superblock(&*source)?;
        if copy != 0 || sb.generation != self.sb.generation || sb.dev_item.devid != devid {
            return Err(Error::UnsupportedFeature(format!(
                "device {devid}'s primary superblock is not the pool's current one; the pool \
                 should be checked before a device of it is replaced"
            )));
        }
        let mut primary = vec![0u8; SUPERBLOCK_SIZE];
        source.read_at(SUPER_OFFSETS[0], &mut primary)?;

        let sectorsize = u64::from(self.sb.sectorsize);
        let size = target.size_bytes() - target.size_bytes() % sectorsize;
        let need = sb.dev_item.total_bytes;
        if size < need {
            return Err(Error::UnsupportedFeature(format!(
                "the replacement holds {size} bytes, fewer than the {need} of device {devid}"
            )));
        }
        let extents = self.device_extents(devid)?;
        if let Some(&(start, len)) = extents
            .iter()
            .find(|&&(start, len)| start.checked_add(len).is_none_or(|end| end > need))
        {
            return Err(Error::UnsupportedFeature(format!(
                "device {devid} has an extent of {len} bytes at {start}, past its {need} bytes"
            )));
        }

        // 1. Every device extent, at the offset it has on the old device.
        let mut buf = vec![0u8; COPY_BYTES];
        for &(start, len) in &extents {
            let mut done = 0u64;
            while done < len {
                let n = usize::try_from(len - done).map_or(COPY_BYTES, |n| n.min(COPY_BYTES));
                source.read_at(start + done, &mut buf[..n])?;
                target.write_at(start + done, &buf[..n])?;
                done += n as u64;
            }
        }
        target.flush()?;

        // 2. The old device's superblock, at every copy the new one holds.
        for &offset in &SUPER_OFFSETS {
            if offset + SUPERBLOCK_SIZE as u64 > target.size_bytes() {
                continue;
            }
            let mut image = primary.clone();
            super_write::set_bytenr(&mut image, offset);
            super_write::stamp_checksum(&mut image, self.sb.csum_type);
            target.write_at(offset, &image)?;
        }
        target.flush()?;

        // 3. The old device is no longer one.
        let blank = vec![0u8; SUPERBLOCK_SIZE];
        for &offset in &SUPER_OFFSETS {
            if offset + SUPERBLOCK_SIZE as u64 > source.size_bytes() {
                continue;
            }
            source.write_at(offset, &blank)?;
        }
        source.flush()?;

        let members = if self.writable_pool.is_empty() {
            vec![target]
        } else {
            self.writable_pool
                .iter()
                .map(|(id, d)| {
                    if *id == devid {
                        Arc::clone(&target)
                    } else {
                        Arc::clone(d)
                    }
                })
                .collect()
        };
        *self = Filesystem::mount_pool_rw(members)?;
        Ok(())
    }

    /// Refuse `device` when it already carries a superblock of this
    /// filesystem: it is a member, or was one, and one disk must not hold
    /// two places in the pool.
    fn refuse_member(&self, device: &Arc<dyn BlockDevice>, what: &str) -> Result<()> {
        if let Ok((sb, _)) = crate::superblock::read_superblock(&**device) {
            if sb.fsid == self.sb.fsid {
                return Err(Error::UnsupportedFeature(format!(
                    "the device {what} already carries a superblock of this filesystem, as \
                     device {}: it is a member, or was one, and must be wiped first",
                    sb.dev_item.devid
                )));
            }
        }
        Ok(())
    }

    /// Every `(physical, length)` the device tree records on device
    /// `devid`, in offset order.
    fn device_extents(&self, devid: u64) -> Result<Vec<(u64, u64)>> {
        let root = self.tree_root(objectid::DEV_TREE)?;
        let reader = self.pool_reader();
        let tree = reader.tree();
        let mut extents = Vec::new();
        let mut bad = None;
        tree.for_each(root, &mut |k: &DiskKey, data: &[u8]| {
            if k.objectid == devid && k.key_type == DEV_EXTENT_KEY {
                match data.get(DEV_EXTENT_LENGTH..DEV_EXTENT_LENGTH + 8) {
                    Some(b) => {
                        extents.push((k.offset, u64::from_le_bytes(b.try_into().expect("8 bytes"))))
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
        Ok(extents)
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
