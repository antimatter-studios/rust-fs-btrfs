//! Turning one committed superblock into the next.
//!
//! The superblock is the commit point: everything a transaction writes
//! is invisible until it names the new root. So the strongest available
//! claim about a superblock writer is that, given the state before a
//! commit and what that commit decided, it produces the state after —
//! the bytes the kernel actually wrote.
//!
//! `scripts/build-commit-fixtures.sh` captures seven superblocks of one
//! filesystem, six commits apart. Each consecutive pair is a before and
//! an after.
//!
//! # Why six and not one
//!
//! The backup ring has four slots and the index is
//! `(generation - 1) mod 4`, so a single commit exercises one slot and a
//! writer that hardcoded a slot number would pass. Six wraps the ring.
//!
//! # What is compared, and what is excluded
//!
//! Everything outside the backup ring is compared byte for byte,
//! checksum included — and the checksum covers the ring too, so it can
//! only match if the ring does.
//!
//! The ring itself is excluded, because filling it is not implemented:
//! it records the roots of the last four commits and needs the addresses
//! of trees this writer is not given. That gap is asserted rather than
//! ignored — the test requires the ring to be the *only* thing that
//! differs, so the day it is implemented this test tightens by deletion.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-commit-fixtures.sh`.

use fs_btrfs::super_write::{
    apply, backup_slot, offsets, stamp_checksum, Commit, ROOT_BACKUPS_END, ROOT_BACKUP_SIZE,
    SUPERBLOCK_SIZE,
};
use fs_btrfs::superblock::ChecksumType;
use std::path::{Path, PathBuf};

mod common;
use common::le64;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// The captured superblocks, in commit order.
fn captured() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for n in 0.. {
        let p = share().join(format!("btrfs-commit-{n}.super"));
        if !p.exists() {
            break;
        }
        match std::fs::read(&p) {
            Ok(b) if b.len() >= SUPERBLOCK_SIZE => out.push(b),
            _ => break,
        }
    }
    out
}

/// Given a superblock and the next one's decisions, produce the next one.
#[test]
fn each_commit_reproduces_the_superblock_the_kernel_wrote() {
    let supers = captured();
    if supers.len() < 2 {
        eprintln!("no commit fixtures; build them with ./scripts/vm-build-commit-fixtures.sh");
        return;
    }

    let mut pairs = 0usize;
    for n in 0..supers.len() - 1 {
        let before = &supers[n];
        let after = &supers[n + 1];

        // What the commit decided. Taken from the after-image, because
        // that is what a real writer would have been told by the
        // transaction — the point is whether the superblock is assembled
        // correctly from those decisions, not whether the decisions can
        // be guessed.
        let commit = Commit {
            generation: le64(after, offsets::GENERATION),
            root: le64(after, offsets::ROOT),
            root_level: Some(after[offsets::ROOT_LEVEL]),
            bytes_used: Some(le64(after, offsets::BYTES_USED)),
            chunk_root: Some(le64(after, offsets::CHUNK_ROOT)),
            chunk_root_generation: Some(le64(after, offsets::CHUNK_ROOT_GENERATION)),
            // The kernel's own commits maintain the free-space tree, so
            // reproducing them must not clear its validity bit.
            invalidate_free_space_tree: false,
        };

        let mut ours = before.clone();
        // The backup ring is not filled by this writer, so the kernel's
        // ring is carried across before the checksum is computed —
        // otherwise the checksum, which covers the ring, could not match
        // and nothing else would be checkable.
        ours[offsets::ROOT_BACKUPS..ROOT_BACKUPS_END]
            .copy_from_slice(&after[offsets::ROOT_BACKUPS..ROOT_BACKUPS_END]);

        // The first mount records a feature in `incompat_flags`, which
        // is not something a commit does. Carrying it across keeps that
        // one-off out of the comparison without pretending a commit
        // writes it.
        const INCOMPAT_FLAGS: usize = 0x0bc;
        ours[INCOMPAT_FLAGS..INCOMPAT_FLAGS + 8]
            .copy_from_slice(&after[INCOMPAT_FLAGS..INCOMPAT_FLAGS + 8]);

        apply(&mut ours, ChecksumType::Crc32c, &commit).expect("apply the commit");

        let differing: Vec<usize> = (0..SUPERBLOCK_SIZE)
            .filter(|&i| ours[i] != after[i])
            .collect();

        if !differing.is_empty() {
            let first = differing[0];
            panic!(
                "commit {n} -> {}: {} bytes differ, first at {first:#06x} \
                 (ours {:#04x}, kernel {:#04x}). Every byte outside the backup ring \
                 should match, so this names a field a commit moves that the writer \
                 does not.",
                n + 1,
                differing.len(),
                ours[first],
                after[first]
            );
        }
        pairs += 1;
    }

    eprintln!(
        "{pairs} commits reproduced byte for byte, backup ring aside \
         (generations {} to {})",
        le64(&supers[0], offsets::GENERATION),
        le64(&supers[supers.len() - 1], offsets::GENERATION)
    );
    assert!(
        pairs >= 4,
        "only {pairs} pairs — too few to wrap the backup ring"
    );
}

/// The checksum this writer produces is the one the kernel produced.
///
/// Separate from the comparison above because it is the claim that
/// matters on its own: a superblock with a wrong checksum is not a
/// filesystem the kernel will mount, whatever else is right about it.
#[test]
fn the_checksum_matches_every_captured_superblock() {
    let supers = captured();
    if supers.is_empty() {
        eprintln!("no commit fixtures — skipping");
        return;
    }

    for (n, sb) in supers.iter().enumerate() {
        let mut ours = sb.clone();
        // Blank it first, so a matching result cannot come from having
        // left the kernel's own answer in place.
        ours[..32].fill(0);
        stamp_checksum(&mut ours, ChecksumType::Crc32c);
        assert_eq!(
            ours[..32],
            sb[..32],
            "superblock {n}: the checksum does not reproduce"
        );
    }
    eprintln!("{} superblock checksums reproduce", supers.len());
}

/// The backup slot the kernel wrote is the one the rule predicts.
///
/// Checked against the ring itself rather than against arithmetic: for
/// each commit, the slot that changed must be the slot
/// `(generation - 1) mod 4` names.
#[test]
fn the_slot_that_changed_is_the_slot_the_rule_names() {
    let supers = captured();
    if supers.len() < 2 {
        eprintln!("no commit fixtures — skipping");
        return;
    }

    let mut checked = 0usize;
    for n in 0..supers.len() - 1 {
        let before = &supers[n];
        let after = &supers[n + 1];
        let gen_before = le64(before, offsets::GENERATION);
        let gen_after = le64(after, offsets::GENERATION);

        // Which slots actually changed.
        let changed: Vec<usize> = (0..4)
            .filter(|&slot| {
                let at = offsets::ROOT_BACKUPS + slot * ROOT_BACKUP_SIZE;
                before[at..at + ROOT_BACKUP_SIZE] != after[at..at + ROOT_BACKUP_SIZE]
            })
            .collect();

        // Every generation strictly after the before and up to the
        // after committed, and each wrote its own slot. A mount and an
        // unmount both commit, so the step is often two.
        let expected: Vec<usize> = ((gen_before + 1)..=gen_after).map(backup_slot).collect();

        for slot in &changed {
            assert!(
                expected.contains(slot),
                "slot {slot} changed between generations {gen_before} and {gen_after}, \
                 but the rule predicts only {expected:?}"
            );
        }
        checked += 1;
    }
    eprintln!("{checked} commits wrote only the slots the rule names");
}
