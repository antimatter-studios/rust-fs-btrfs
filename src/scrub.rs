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

use crate::error::{Error, Result};
use crate::fs::Filesystem;

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
    /// Always, for now: nothing reads the copies yet.
    pub fn scrub(&self) -> Result<ScrubReport> {
        Err(Error::UnsupportedFeature(
            "scrub is not implemented yet".into(),
        ))
    }
}
