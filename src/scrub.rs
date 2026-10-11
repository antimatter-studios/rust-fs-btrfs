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
//!
//! A RAID5/6 chunk has one copy and parity, which no read looks at while
//! the data verifies, so a damaged P or Q would go unnoticed until the
//! data it protects is lost. Every full stripe holding allocated bytes
//! has its parity recomputed from its data elements and compared, sector
//! by sector where some data element holds allocated bytes: the sectors
//! the kernel's scrub checks (#301). A full stripe whose data is itself
//! damaged is left to the data's own report: its parity is computed from
//! bytes known to be wrong.

use crate::btree::{TreeBlock, TreeGeometry};
use crate::chunk::{key_type, objectid, Chunk, DiskKey};
use crate::error::Result;
use crate::fs::Filesystem;
use std::collections::BTreeSet;

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
    /// RAID5/6 full stripes holding allocated bytes whose parity was
    /// recomputed and compared.
    pub parity_stripes: u64,
    /// Every bad copy, in address order.
    pub errors: Vec<ScrubError>,
}

/// One copy of one block or sector that is not what was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubError {
    /// The logical address: the tree block's, the data sector's, or for
    /// a parity element the start of its full stripe.
    pub logical: u64,
    /// Which copy, from 0; for a parity element, 0 for P and 1 for Q.
    pub mirror: usize,
    /// Whether it is a tree block or data.
    pub what: ScrubTarget,
    /// What is wrong with it.
    pub detail: String,
    /// Whether another copy of the same bytes verifies, so the damage is
    /// recoverable; for a parity element, whether the data it is computed
    /// from verifies. Nothing here repairs it.
    pub repairable: bool,
    /// The device and byte offset on it of the element that is wrong,
    /// when the error is about one element on one device: a parity
    /// element, which has no logical address of its own.
    pub device: Option<(u64, u64)>,
}

/// Which kind of block a [`ScrubError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScrubTarget {
    /// A tree block.
    TreeBlock,
    /// A data sector.
    Data,
    /// A RAID5/6 parity element, P or Q, that is not what the data of its
    /// full stripe computes to.
    Parity,
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

        // What is allocated, as sorted, merged [start, end) ranges: the
        // sectors of a full stripe whose parity is worth comparing.
        let mut allocated: Vec<(u64, u64)> = trees
            .iter()
            .map(|&at| (at, at.saturating_add(nodesize)))
            .chain(data.iter().map(|&(at, len)| (at, at.saturating_add(len))))
            .collect();
        allocated.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(allocated.len());
        for (start, end) in allocated {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }

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
        self.scrub_parity(&merged, &mut report)?;
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
                device: None,
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
                    device: None,
                });
            }
        }
        Ok(())
    }

    /// Every RAID5/6 full stripe holding any allocated byte, its parity
    /// checked; see the module notes.
    fn scrub_parity(&self, allocated: &[(u64, u64)], report: &mut ScrubReport) -> Result<()> {
        let mut stripes: BTreeSet<(u64, u64)> = BTreeSet::new();
        for &(start, end) in allocated {
            let mut at = start;
            while at < end {
                let Some(chunk) = self.chunk_map().chunk_for(at) else {
                    break;
                };
                let upto = chunk.logical_end().min(end);
                if let Some(parity) = chunk.parity_count() {
                    let full = full_stripe_len(chunk, parity);
                    let first = (at - chunk.logical) / full;
                    let last = (upto - 1 - chunk.logical) / full;
                    stripes.extend((first..=last).map(|f| (chunk.logical, f)));
                }
                at = upto;
            }
        }
        for (logical, full) in stripes {
            let chunk = self
                .chunk_map()
                .chunk_for(logical)
                .expect("the chunk each full stripe was found in")
                .clone();
            self.scrub_full_stripe(&chunk, full, allocated, report)?;
        }
        Ok(())
    }

    fn scrub_full_stripe(
        &self,
        chunk: &Chunk,
        full: u64,
        allocated: &[(u64, u64)],
        report: &mut ScrubReport,
    ) -> Result<()> {
        let parity = chunk.parity_count().expect("a parity chunk");
        let span = full_stripe_len(chunk, parity);
        let start = chunk.logical + full * span;
        let plan = chunk.parity_read(start)?;
        report.parity_stripes += 1;

        // DATA THAT IS ITSELF DAMAGED is the data scrub's to report, and
        // parity computed from it says nothing about the parity on disk.
        if report
            .errors
            .iter()
            .any(|e| e.what != ScrubTarget::Parity && (start..start + span).contains(&e.logical))
        {
            return Ok(());
        }
        let len = plan.len as usize;
        let mut data = Vec::with_capacity(plan.data.len());
        for &(devid, physical) in &plan.data {
            let mut buf = vec![0u8; len];
            // An element that cannot be read is the data scrub's to report.
            if self.read_element(devid, physical, &mut buf).is_err() {
                return Ok(());
            }
            data.push(buf);
        }
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let (p, q) = crate::raid56::parity(&refs, plan.q.is_some());

        // The sectors, by offset into the element, where some data element
        // holds allocated bytes.
        let sector = self.superblock().sectorsize as usize;
        let checked: Vec<usize> = (0..len / sector)
            .filter(|&s| {
                (0..data.len() as u64).any(|i| {
                    let at = start + i * chunk.stripe_len + (s * sector) as u64;
                    overlaps(allocated, at, sector as u64)
                })
            })
            .collect();
        if checked.is_empty() {
            return Ok(());
        }

        for (mirror, (want, at)) in [(Some(p), Some(plan.p)), (q, plan.q)]
            .into_iter()
            .enumerate()
        {
            let (Some(want), Some((devid, physical))) = (want, at) else {
                continue;
            };
            let name = if mirror == 0 { "P" } else { "Q" };
            let mut got = vec![0u8; len];
            let detail = match self.read_element(devid, physical, &mut got) {
                Err(e) => Some(format!("{name} cannot be read: {e}")),
                Ok(()) => {
                    let bad: Vec<usize> = checked
                        .iter()
                        .copied()
                        .filter(|&s| {
                            got[s * sector..(s + 1) * sector] != want[s * sector..(s + 1) * sector]
                        })
                        .collect();
                    bad.first().map(|first| {
                        format!(
                            "{name} differs from what the data computes to in {} of {} sectors, \
                             the first at byte {} of the element",
                            bad.len(),
                            checked.len(),
                            first * sector
                        )
                    })
                }
            };
            if let Some(detail) = detail {
                report.errors.push(ScrubError {
                    logical: start,
                    mirror,
                    what: ScrubTarget::Parity,
                    detail,
                    // The data verified above, so the parity can be
                    // computed again from it.
                    repairable: true,
                    device: Some((devid, physical)),
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

/// The logical bytes one full stripe of a parity chunk holds: its data
/// elements, `num_stripes - parity` of them.
fn full_stripe_len(chunk: &Chunk, parity: usize) -> u64 {
    (u64::from(chunk.num_stripes) - parity as u64) * chunk.stripe_len
}

/// Whether `[at, at + len)` meets any of `allocated`, which is sorted and
/// merged.
fn overlaps(allocated: &[(u64, u64)], at: u64, len: u64) -> bool {
    let i = allocated.partition_point(|&(_, end)| end <= at);
    allocated.get(i).is_some_and(|&(start, _)| start < at + len)
}
