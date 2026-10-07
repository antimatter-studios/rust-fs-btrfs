//! Trim: every free range of every block group, on every copy, handed to
//! a discard (#305).
//!
//! A filesystem knows which of its bytes hold nothing; the device under
//! it does not, and an SSD or a thin-provisioned volume keeps every
//! block it was ever written as in use until told otherwise. Trim tells
//! it: for each run the filesystem has free, the device range each copy
//! of that run occupies is discarded.
//!
//! What is free is the free-space tree's answer, the same record the
//! kernel's allocator and `fstrim` use, and the extent tree's gaps on a
//! volume that has no free-space tree. A free logical run maps to one
//! device range per copy: two for DUP and RAID1, three and four for
//! RAID1C3/C4, split wherever a stripe ends. Bytes no chunk maps (the
//! unallocated tail of a device) are not part of any block group and are
//! not discarded here.
//!
//! The discard itself is the caller's: [`fs_core::BlockDevice`] has no
//! discard operation, so [`Filesystem::trim`] hands each range to a
//! function that issues it (`BLKDISCARD`, `fallocate(PUNCH_HOLE)` on an
//! image, or a test's list), and changes nothing on the filesystem.

use crate::error::{Error, Result};
use crate::fs::Filesystem;

/// One device range a trim discards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrimRange {
    /// Where the free run starts, in logical address space.
    pub logical: u64,
    /// Which copy of it this is, from 0.
    pub mirror: usize,
    /// The device the copy is on, as chunk stripes name it.
    pub devid: u64,
    /// Where on that device.
    pub physical: u64,
    /// How many bytes.
    pub len: u64,
}

/// What a trim discarded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrimReport {
    /// Every range handed to the discard, in the order it was.
    pub ranges: Vec<TrimRange>,
    /// Their total length, counting each copy.
    pub bytes: u64,
}

impl Filesystem {
    /// Every device range a trim of this filesystem discards, in logical
    /// order and copy by copy: each free run of each block group, mapped
    /// to every copy of it.
    ///
    /// # Errors
    ///
    /// Propagates a tree read failure and a free run no chunk maps.
    /// A RAID5/6 group is refused: its free space is shared with parity
    /// that covers allocated bytes, and discarding it is not a matter of
    /// mapping a range.
    pub fn trim_ranges(&self) -> Result<Vec<TrimRange>> {
        let mut out = Vec::new();
        for group in self.block_groups()? {
            if group.flags & (crate::chunk::block_group::RAID5 | crate::chunk::block_group::RAID6)
                != 0
            {
                return Err(Error::UnsupportedFeature(format!(
                    "the block group at {} is RAID5/6, which trim does not discard",
                    group.start
                )));
            }
            let free = match self.cached_free_extents(&group)? {
                Some(free) => free,
                None => self.free_extents(&group)?,
            };
            for run in free {
                let copies = self.chunk_map().mirrors_at(run.start)?;
                for mirror in 0..copies {
                    let mut done = 0u64;
                    while done < run.len {
                        let m = self.chunk_map().map_mirror(run.start + done, mirror)?;
                        let n = m.len.min(run.len - done);
                        if n == 0 {
                            return Err(Error::UnmappedLogical(run.start + done));
                        }
                        out.push(TrimRange {
                            logical: run.start + done,
                            mirror,
                            devid: m.devid,
                            physical: m.physical,
                            len: n,
                        });
                        done += n;
                    }
                }
            }
        }
        Ok(out)
    }

    /// Discard every free range of this filesystem, through `discard`.
    ///
    /// The ranges are [`Filesystem::trim_ranges`], all resolved before
    /// the first is discarded, so a filesystem whose free space cannot be
    /// worked out discards nothing. Nothing on the filesystem changes:
    /// the bytes discarded are ones no tree points at.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`]:
    /// a discard throws bytes away, which is a write. Otherwise as
    /// [`Filesystem::trim_ranges`], and the first error `discard` returns,
    /// with the ranges before it discarded.
    pub fn trim(&self, discard: &mut dyn FnMut(&TrimRange) -> Result<()>) -> Result<TrimReport> {
        if !self.is_writable() {
            return Err(Error::ReadOnly);
        }
        let ranges = self.trim_ranges()?;
        let mut bytes = 0u64;
        for range in &ranges {
            discard(range)?;
            bytes += range.len;
        }
        Ok(TrimReport { ranges, bytes })
    }
}
