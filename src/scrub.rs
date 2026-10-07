//! Scrub, read-only: every copy of every allocated block read and checked
//! against its checksum, nothing repaired (#268).
//!
//! A read uses the first copy that verifies and never looks at the rest,
//! so a second copy can be bad for years without anything noticing —
//! until the first goes too. Scrub reads them all. It is what
//! `btrfs scrub start -r` does: verify, report, change nothing.
//!
//! What is read is what the extent tree says is allocated: every tree
//! block (`METADATA_ITEM`, or an `EXTENT_ITEM` flagged as a tree block),
//! checked as a tree block — checksum, then that the header names the
//! address it was read from and this filesystem — and every data extent,
//! sector by sector against the checksum tree. A data sector the checksum
//! tree does not cover (a `nodatasum` file) is read but cannot be judged,
//! as on the kernel.
//!
//! Each copy is a mirror index as the read path counts them: two for DUP
//! and RAID1, three and four for RAID1C3/C4, `sub_stripes` for RAID10.

use crate::btree::{TreeBlock, TreeGeometry};
use crate::chunk::{key_type, objectid, DiskKey};
use crate::error::Result;
use crate::fs::Filesystem;

/// `BTRFS_EXTENT_FLAG_TREE_BLOCK`, in an `EXTENT_ITEM`'s flags.
const EXTENT_FLAG_TREE_BLOCK: u64 = 1 << 1;

/// How much of a data extent is read at a time.
const DATA_WINDOW: u64 = 1024 * 1024;

/// What a scrub found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScrubReport {
    /// Tree blocks checked, counting each copy once.
    pub tree_blocks: u64,
    /// Data bytes read, counting each copy once.
    pub data_bytes: u64,
    /// Data sectors no checksum covers, counting each copy once: read,
    /// and not judged.
    pub data_unverified: u64,
    /// Every bad copy, in address order.
    pub errors: Vec<ScrubError>,
}

/// One copy of one block or sector that is not what was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubError {
    /// The logical address: the tree block's, or the data sector's.
    pub logical: u64,
    /// Which copy, from 0.
    pub mirror: usize,
    /// Whether it is a tree block or data.
    pub what: ScrubTarget,
    /// What is wrong with it.
    pub detail: String,
    /// Whether another copy of the same bytes verifies, so the damage is
    /// recoverable. Nothing here repairs it.
    pub repairable: bool,
}

/// Which kind of block a [`ScrubError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScrubTarget {
    /// A tree block.
    TreeBlock,
    /// A data sector.
    Data,
}

impl Filesystem {
    /// Read every copy of every allocated block and report the ones that
    /// do not verify. Changes nothing.
    ///
    /// # Errors
    ///
    /// When the extent tree itself cannot be walked: then there is no
    /// list of what to check. A copy that cannot be read is a
    /// [`ScrubError`], not a failure of the scrub.
    pub fn scrub(&self) -> Result<ScrubReport> {
        let nodesize = u64::from(self.superblock().nodesize);
        let mut trees: Vec<u64> = Vec::new();
        let mut data: Vec<(u64, u64)> = Vec::new();
        let root = self.tree_root_public(objectid::EXTENT_TREE)?;
        self.for_each_item_in(root, &mut |k: &DiskKey, item: &[u8]| {
            if k.key_type == key_type::METADATA_ITEM {
                trees.push(k.objectid);
            } else if k.key_type == key_type::EXTENT_ITEM {
                let flags = item
                    .get(16..24)
                    .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")));
                if flags & EXTENT_FLAG_TREE_BLOCK != 0 {
                    trees.push(k.objectid);
                } else {
                    data.push((k.objectid, k.offset));
                }
            }
        })?;

        let mut report = ScrubReport::default();
        let geom = TreeGeometry::from_superblock(self.superblock());
        for at in trees {
            self.scrub_tree_block(at, nodesize, &geom, &mut report)?;
        }
        for (start, len) in data {
            let mut done = 0;
            while done < len {
                let n = (len - done).min(DATA_WINDOW);
                self.scrub_data(start + done, n, &mut report)?;
                done += n;
            }
        }
        report.errors.sort_by_key(|e| (e.logical, e.mirror));
        Ok(report)
    }

    fn scrub_tree_block(
        &self,
        at: u64,
        nodesize: u64,
        geom: &TreeGeometry,
        report: &mut ScrubReport,
    ) -> Result<()> {
        let copies = self.chunk_map().mirrors_at(at)?;
        let mut bad = Vec::new();
        for mirror in 0..copies {
            report.tree_blocks += 1;
            let mut buf = vec![0u8; nodesize as usize];
            let verdict = self
                .read_copy(at, mirror, &mut buf)
                .and_then(|()| TreeBlock::parse(buf, at, geom).map(|_| ()));
            if let Err(e) = verdict {
                bad.push((mirror, e.to_string()));
            }
        }
        let repairable = bad.len() < copies;
        for (mirror, detail) in bad {
            report.errors.push(ScrubError {
                logical: at,
                mirror,
                what: ScrubTarget::TreeBlock,
                detail,
                repairable,
            });
        }
        Ok(())
    }

    fn scrub_data(&self, start: u64, len: u64, report: &mut ScrubReport) -> Result<()> {
        let sector = u64::from(self.superblock().sectorsize);
        let digests = self.data_digests(start, len)?;
        let copies = self.chunk_map().mirrors_at(start)?;
        let sectors = len.div_ceil(sector);
        // Per sector: the copies that failed, and why.
        let mut bad: Vec<Vec<(usize, String)>> = vec![Vec::new(); sectors as usize];
        for mirror in 0..copies {
            let mut buf = vec![0u8; (sectors * sector) as usize];
            report.data_bytes += len;
            if let Err(e) = self.read_copy(start, mirror, &mut buf[..len as usize]) {
                for b in &mut bad {
                    b.push((mirror, e.to_string()));
                }
                continue;
            }
            for (i, chunk) in buf.chunks_exact(sector as usize).enumerate() {
                let at = start + i as u64 * sector;
                match digests.get(&at) {
                    None => report.data_unverified += 1,
                    Some(want) if !self.superblock().csum_type.verify(chunk, want) => {
                        bad[i].push((mirror, "checksum mismatch".to_string()));
                    }
                    Some(_) => {}
                }
            }
        }
        for (i, failed) in bad.into_iter().enumerate() {
            let repairable = failed.len() < copies;
            for (mirror, detail) in failed {
                report.errors.push(ScrubError {
                    logical: start + i as u64 * sector,
                    mirror,
                    what: ScrubTarget::Data,
                    detail,
                    repairable,
                });
            }
        }
        Ok(())
    }

    /// Copy `mirror` of `buf.len()` bytes at `at`, nothing verified.
    fn read_copy(&self, at: u64, mirror: usize, buf: &mut [u8]) -> Result<()> {
        self.read_mirror_unverified(at, mirror, buf)
    }
}
