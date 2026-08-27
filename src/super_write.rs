//! Committing: writing the superblock that makes a transaction real.
//!
//! Everything a Btrfs transaction writes is invisible until the
//! superblock names the new root. Tree blocks can be written, flushed
//! and still count for nothing; the superblock is where a commit happens
//! and the only place it happens.
//!
//! That makes this the most consequential 4 KiB in the filesystem, and
//! the one where being approximately right is worst: a superblock naming
//! a root that was never written, or carrying a stale checksum, does not
//! fail to mount — it mounts against the wrong tree.
//!
//! # What a commit actually changes
//!
//! Measured, not taken from the field table. Six consecutive commits of
//! one real filesystem were captured and compared pairwise. Outside the
//! backup ring, the steady state moves exactly four things:
//!
//! ```text
//! 0x0000  csum                  every commit
//! 0x0048  generation            every commit
//! 0x0050  root                  every commit
//! 0x0233  uuid_tree_generation  every commit, tracking generation
//! ```
//!
//! `bytes_used`, `root_level`, `chunk_root` and `cache_generation` did
//! not move across any of those six, which agrees with the field table's
//! "when usage changes", "when the root tree changes depth" and "when a
//! chunk is allocated" — this workload did none of the three. They are
//! settable here because a commit that does one of those must set them.
//!
//! One field moved once and never again: `incompat_flags`, `0x341` to
//! `0x361` between the state `mkfs` left and the first commit. That is
//! the first mount recording a feature, not something a commit does, and
//! a writer that copied the behaviour would be setting a flag it does
//! not implement.
//!
//! # The checksum
//!
//! Over bytes `[32..4096]` — everything after the checksum itself, the
//! same rule tree blocks use. Reproduced on all seven captured
//! superblocks before anything here was written.

use crate::error::{Error, Result};
use crate::superblock::{ChecksumType, CSUM_SIZE};

/// Byte offsets within the superblock.
///
/// From `docs/transaction-format.md`, which measured them, and confirmed
/// against the captured commits.
pub mod offsets {
    /// The checksum, over everything after it.
    pub const CSUM: usize = 0x000;
    /// The transaction this superblock commits.
    pub const GENERATION: usize = 0x048;
    /// The root tree's address — what makes the commit real.
    pub const ROOT: usize = 0x050;
    /// The chunk tree's address. Moves only when a chunk is allocated.
    pub const CHUNK_ROOT: usize = 0x058;
    /// Bytes in use. Moves only when usage changes, and must equal the
    /// sum of every `BLOCK_GROUP_ITEM.used`.
    pub const BYTES_USED: usize = 0x078;
    /// The chunk tree's generation.
    pub const CHUNK_ROOT_GENERATION: usize = 0x0a4;
    /// Height of the root tree. Moves only when its depth changes.
    pub const ROOT_LEVEL: usize = 0x0c6;
    /// Zero with the free-space tree; tracks generation under
    /// `space_cache=v1`.
    pub const CACHE_GENERATION: usize = 0x22b;
    /// Tracks generation even when the UUID tree is untouched.
    pub const UUID_TREE_GENERATION: usize = 0x233;
    /// The first of four `btrfs_root_backup` slots.
    pub const ROOT_BACKUPS: usize = 0xb2b;
}

/// One `btrfs_root_backup` slot.
pub const ROOT_BACKUP_SIZE: usize = 168;

/// How many backup slots the ring holds.
pub const ROOT_BACKUP_SLOTS: u64 = 4;

/// One past the last byte of the backup ring.
pub const ROOT_BACKUPS_END: usize =
    offsets::ROOT_BACKUPS + (ROOT_BACKUP_SLOTS as usize) * ROOT_BACKUP_SIZE;

/// The whole superblock.
pub const SUPERBLOCK_SIZE: usize = 4096;

/// Which backup slot a generation writes.
///
/// `(generation - 1) mod 4`. The transaction document confirmed it
/// across six independent generations, and the captured commits confirm
/// it again from the other direction: a filesystem going from generation
/// 6 to 8 — a mount and an unmount, each committing — changed slots 2
/// and 3, which is `(7-1) mod 4` and `(8-1) mod 4`.
///
/// Generation 0 is not a commit and has no slot, so it is not special
/// cased: nothing calls this with one.
pub fn backup_slot(generation: u64) -> usize {
    ((generation.wrapping_sub(1)) % ROOT_BACKUP_SLOTS) as usize
}

/// Where a generation's backup slot begins.
pub fn backup_slot_offset(generation: u64) -> usize {
    offsets::ROOT_BACKUPS + backup_slot(generation) * ROOT_BACKUP_SIZE
}

/// What a commit sets in the superblock.
///
/// Only `generation` and `root` are required of every commit. The rest
/// are `None` when the commit does not move them, which is the common
/// case — a commit that changes no usage, no tree depth and allocates no
/// chunk leaves all four alone, and writing them back unchanged would be
/// the same bytes but a worse description of what happened.
#[derive(Debug, Clone, Default)]
pub struct Commit {
    /// The new transaction number. Every commit.
    pub generation: u64,
    /// The new root tree address. Every commit.
    pub root: u64,
    /// Set when the root tree's depth changed.
    pub root_level: Option<u8>,
    /// Set when usage changed. Must equal the sum of every
    /// `BLOCK_GROUP_ITEM.used`.
    pub bytes_used: Option<u64>,
    /// Set when a chunk was allocated.
    pub chunk_root: Option<u64>,
    /// Set alongside `chunk_root`.
    pub chunk_root_generation: Option<u64>,
}

/// Apply a commit to a superblock, in place, and re-checksum it.
///
/// The result is the superblock as it should be written to every copy —
/// except for each copy's own `bytenr`, which is that copy's address and
/// is not this function's business.
///
/// # What it does not do
///
/// **It does not fill the backup slot.** The ring records the roots of
/// each of the last four commits, and doing that needs the addresses of
/// trees this does not take — the extent, device and checksum roots
/// among them. The slot is left as it was, which is a slot describing an
/// older commit rather than a wrong description of this one.
///
/// That is a real gap and it is named rather than hidden: the backups
/// are what `btrfs rescue` reads when the primary root will not parse,
/// so a filesystem committed by this function is recoverable only as far
/// back as the last commit the kernel made.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] if the buffer is not a whole
/// superblock.
pub fn apply(raw: &mut [u8], csum_type: ChecksumType, commit: &Commit) -> Result<()> {
    if raw.len() < SUPERBLOCK_SIZE {
        return Err(Error::UnsupportedFeature(format!(
            "a superblock is {SUPERBLOCK_SIZE} bytes and this is {}",
            raw.len()
        )));
    }

    put64(raw, offsets::GENERATION, commit.generation);
    put64(raw, offsets::ROOT, commit.root);

    // Tracks generation even when the UUID tree itself was not touched,
    // which is why it is not optional and not derived from anything the
    // caller passes.
    put64(raw, offsets::UUID_TREE_GENERATION, commit.generation);

    if let Some(level) = commit.root_level {
        raw[offsets::ROOT_LEVEL] = level;
    }
    if let Some(used) = commit.bytes_used {
        put64(raw, offsets::BYTES_USED, used);
    }
    if let Some(chunk_root) = commit.chunk_root {
        put64(raw, offsets::CHUNK_ROOT, chunk_root);
    }
    if let Some(gen) = commit.chunk_root_generation {
        put64(raw, offsets::CHUNK_ROOT_GENERATION, gen);
    }

    stamp_checksum(raw, csum_type);
    Ok(())
}

/// Compute and store the superblock's checksum.
///
/// Over everything after the checksum itself, so it can only be done
/// once every other field is final. Same rule as a tree block.
pub fn stamp_checksum(raw: &mut [u8], csum_type: ChecksumType) {
    let digest = csum_type.digest(&raw[CSUM_SIZE..SUPERBLOCK_SIZE]);
    raw[..CSUM_SIZE].copy_from_slice(&digest);
}

fn put64(raw: &mut [u8], at: usize, value: u64) {
    raw[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot rule, including the wrap that a single commit would not
    /// exercise.
    #[test]
    fn the_backup_slot_is_the_generation_before_it_modulo_four() {
        assert_eq!(backup_slot(1), 0);
        assert_eq!(backup_slot(2), 1);
        assert_eq!(backup_slot(3), 2);
        assert_eq!(backup_slot(4), 3);
        assert_eq!(backup_slot(5), 0, "and round again");

        // The pair the captured commits showed: generation 6 to 8 wrote
        // slots 2 and 3.
        assert_eq!(backup_slot(7), 2);
        assert_eq!(backup_slot(8), 3);
    }

    /// Slot offsets are 168 apart and the ring stays inside the
    /// superblock.
    #[test]
    fn the_ring_fits_in_the_superblock() {
        assert_eq!(backup_slot_offset(1), offsets::ROOT_BACKUPS);
        assert_eq!(
            backup_slot_offset(2),
            offsets::ROOT_BACKUPS + ROOT_BACKUP_SIZE
        );
        let last = backup_slot_offset(4) + ROOT_BACKUP_SIZE;
        assert!(
            last <= SUPERBLOCK_SIZE,
            "the four slots end at {last}, past the superblock"
        );
    }

    /// A buffer that is not a superblock is refused rather than indexed
    /// into.
    #[test]
    fn a_short_buffer_is_refused() {
        let mut small = vec![0u8; 512];
        assert!(apply(&mut small, ChecksumType::Crc32c, &Commit::default()).is_err());
    }
}
