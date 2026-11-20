//! Every superblock copy, each with its own verdict, and the backup roots
//! each one records.
//!
//! [`crate::superblock::read_superblock`] answers the mount's question:
//! which one copy to trust. Salvage asks a different one: what does each
//! copy say, and what is wrong with it. A copy whose checksum fails still
//! names a generation, tree roots and four backup slots, and those are
//! what an older-generation walk starts from when the newest roots are
//! damaged.

use crate::error::Error;
use crate::super_write::{read_backups, BackupRoots};
use crate::superblock::{Superblock, SUPER_INFO_SIZE, SUPER_OFFSETS};
use fs_core::BlockRead;

/// What is wrong with one superblock copy, or that nothing is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyVerdict {
    /// The copy verifies: magic, checksum, its own offset, the incompat
    /// features and the geometry.
    Valid,
    /// The device ends before this copy would. Not damage: a volume under
    /// 256 GiB has no third copy.
    Absent,
    /// The device failed the read.
    Unreadable(String),
    /// The magic is not Btrfs: the copy was overwritten or never written.
    NotBtrfs,
    /// The checksum type is one no algorithm exists for, so nothing in
    /// the copy can be checked or decoded.
    UnknownChecksumType(u16),
    /// The stored checksum disagrees with the bytes. The fields still
    /// decode, and [`SuperblockCopy::superblock`] holds them.
    ChecksumMismatch,
    /// The copy verifies but records another offset as its own: a stale
    /// superblock from a filesystem laid out differently.
    Misplaced {
        /// The offset the copy names.
        found: u64,
    },
    /// The copy verifies but declares an incompatible feature this crate
    /// cannot read. Intact, so not damage, but nothing past it is safe to
    /// interpret.
    UnsupportedFeature(String),
    /// The copy verifies but a field is out of range or inconsistent.
    Malformed(String),
}

impl CopyVerdict {
    fn of(error: Error) -> Self {
        match error {
            Error::NotBtrfs { .. } => Self::NotBtrfs,
            Error::UnsupportedChecksum(raw) => Self::UnknownChecksumType(raw),
            Error::ChecksumMismatch { .. } => Self::ChecksumMismatch,
            Error::BlockIdentityMismatch { found, .. } => Self::Misplaced { found },
            Error::UnsupportedFeature(m) => Self::UnsupportedFeature(m),
            Error::Io(m) => Self::Unreadable(m),
            other => Self::Malformed(other.to_string()),
        }
    }
}

/// One superblock copy as found on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperblockCopy {
    /// Which copy: 0 is the primary at 64 KiB, 1 at 64 MiB, 2 at 256 GiB.
    pub mirror: usize,
    /// The physical offset the copy lives at.
    pub offset: u64,
    pub verdict: CopyVerdict,
    /// The decoded fields, present whenever the bytes were read and hold a
    /// Btrfs magic and a known checksum type, whatever the verdict. Only a
    /// [`CopyVerdict::Valid`] copy's fields are verified.
    pub superblock: Option<Superblock>,
    /// The four backup slots in slot order, filled or not
    /// ([`BackupRoots::is_empty`]). Present exactly when `superblock` is.
    pub backups: Vec<BackupRoots>,
}

impl SuperblockCopy {
    /// The copy's generation, as decoded, whether or not it verifies.
    pub fn generation(&self) -> Option<u64> {
        self.superblock.as_ref().map(|sb| sb.generation)
    }

    /// The filled backup slots, newest tree-root generation first.
    pub fn backups_newest_first(&self) -> Vec<BackupRoots> {
        let mut filled: Vec<BackupRoots> = self
            .backups
            .iter()
            .filter(|b| !b.is_empty())
            .copied()
            .collect();
        filled.sort_by_key(|b| std::cmp::Reverse(b.tree.generation));
        filled
    }
}

/// Every superblock copy the format defines, in mirror order, each with its
/// verdict. Always three entries; a copy past the device's end is
/// [`CopyVerdict::Absent`].
///
/// Reads each copy once, and nothing else. A failed read is a verdict on
/// that copy, not an error, so this never fails.
pub fn superblock_copies(source: &dyn BlockRead) -> Vec<SuperblockCopy> {
    let size = source.size_bytes();
    SUPER_OFFSETS
        .iter()
        .enumerate()
        .map(|(mirror, &offset)| {
            let absent = SuperblockCopy {
                mirror,
                offset,
                verdict: CopyVerdict::Absent,
                superblock: None,
                backups: Vec::new(),
            };
            if offset.saturating_add(SUPER_INFO_SIZE as u64) > size {
                return absent;
            }
            let mut raw = vec![0u8; SUPER_INFO_SIZE];
            if let Err(error) = source.read_at(offset, &mut raw) {
                return SuperblockCopy {
                    verdict: CopyVerdict::Unreadable(error.to_string()),
                    ..absent
                };
            }
            let verdict = match Superblock::parse_at(&raw, offset) {
                Ok(_) => CopyVerdict::Valid,
                Err(error) => CopyVerdict::of(error),
            };
            let superblock = Superblock::parse_unverified(&raw).ok();
            let backups = if superblock.is_some() {
                read_backups(&raw)
            } else {
                Vec::new()
            };
            SuperblockCopy {
                verdict,
                superblock,
                backups,
                ..absent
            }
        })
        .collect()
}

/// The copy a mount would choose: the valid one with the highest
/// generation, the lower mirror on a tie. `None` when no copy is valid.
pub fn best_copy(copies: &[SuperblockCopy]) -> Option<&SuperblockCopy> {
    copies
        .iter()
        .filter(|copy| copy.verdict == CopyVerdict::Valid)
        .fold(None, |best: Option<&SuperblockCopy>, copy| match best {
            Some(b) if b.generation() >= copy.generation() => Some(b),
            _ => Some(copy),
        })
}
