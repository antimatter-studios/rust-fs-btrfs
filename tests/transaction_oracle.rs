//! What must be true of a transaction, whatever it was for.
//!
//! The remaining piece of the write path decides which blocks a change
//! produces. That decision is recursive — recording an allocation
//! modifies the extent tree, which lives in blocks that must themselves
//! be allocated — and it is the part where a plausible implementation
//! can be wrong in ways that still mount.
//!
//! So the checks come first. These are asserted against the KERNEL's
//! transactions, on before/after image pairs it produced, and they are
//! written before the planner exists precisely so the planner has
//! something to satisfy that was not shaped around it. When it lands,
//! its output goes through the same three assertions.
//!
//! `docs/cow-transaction.md` records what the pairs showed. The short
//! version: an empty commit is not empty — it rewrites the root, extent,
//! free-space and dev trees, four blocks in and four out — and a `touch`
//! costs that floor plus two fs-tree blocks.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-cow-fixtures.sh`.

use fs_btrfs::btree::header_offsets as o;
use fs_btrfs::chunk::{key_type, DiskKey};
use fs_btrfs::fs::Filesystem;
use fs_core::FileDevice;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn image(name: &str) -> Option<PathBuf> {
    let p = share().join(name);
    p.exists().then_some(p)
}

fn mount(p: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open(p).expect("opening a fixture"));
    Filesystem::mount(dev).expect("mounting a fixture")
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The addresses named by a `METADATA_ITEM`, i.e. the tree blocks the
/// filesystem believes are allocated.
fn recorded_blocks(fs: &Filesystem) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    fs.for_each_extent_item(&mut |k: &DiskKey, _: &[u8]| {
        if k.key_type == key_type::METADATA_ITEM {
            out.insert(k.objectid);
        }
    })
    .expect("walking the extent tree");
    out
}

/// Every tree block present in an image whose checksum verifies, as
/// (address, generation, owner).
///
/// Found by scanning rather than walking, so it reaches blocks the
/// current root no longer points at — which is exactly the set a
/// transaction leaves behind.
///
/// Deduplicated by logical address, because the scan is physical and a
/// DUP filesystem stores every block twice. Counting both copies is not
/// wrong so much as a different question: four blocks written to a
/// mirrored filesystem is eight writes and four blocks, and it is the
/// blocks that a transaction is measured in.
fn present_blocks(fs: &Filesystem, path: &Path) -> BTreeSet<(u64, u64, u64)> {
    let sb = fs.superblock();
    let bytes = std::fs::read(path).expect("reading a fixture");
    let n = sb.nodesize as usize;
    let mut out = BTreeSet::new();
    let mut at = 0usize;
    while at + n <= bytes.len() {
        let b = &bytes[at..at + n];
        at += n;
        if b[o::FSID..o::FSID + 16] != sb.fsid[..] {
            continue;
        }
        if !sb.csum_type.verify(&b[32..], &b[..32]) {
            continue;
        }
        out.insert((
            le64(b, o::BYTENR),
            le64(b, o::GENERATION),
            le64(b, o::OWNER),
        ));
    }
    out
}

/// Whether a pair actually contains a transaction.
///
/// A mount cycle that changes nothing DOES NOT ALWAYS COMMIT. It did in
/// the Debian oracle VM, where the control pair was measured, and it did
/// not on the CI runner — same script, same fixture, generation
/// unmoved. Which is fair: there was nothing to write.
///
/// So a pair with no transaction in it is not a failure, it is a pair
/// with nothing to check. The tests below skip those and require that
/// something, somewhere, did commit — otherwise a fixture builder that
/// silently stopped producing transactions would read as a pass.
fn committed(before: &Path, after: &Path) -> bool {
    mount(after).superblock().generation > mount(before).superblock().generation
}

fn pairs() -> Vec<(&'static str, PathBuf, PathBuf)> {
    let mut out = Vec::new();
    if let (Some(b), Some(c)) = (
        image("btrfs-cow-before.img"),
        image("btrfs-cow-control.img"),
    ) {
        out.push(("an empty commit", b, c));
    }
    if let (Some(b), Some(a)) = (image("btrfs-cow-before.img"), image("btrfs-cow-after.img")) {
        out.push(("one touch", b, a));
    }
    out
}

/// Every block the LAST commit wrote is recorded as allocated.
///
/// A block written and not recorded is one the allocator will hand out
/// again — and the filesystem will still mount, read correctly, and
/// corrupt itself on the next transaction. This is the invariant that
/// makes allocation mean anything.
///
/// It applies to the last commit's blocks, not to everything newer than
/// the before image. A pair may span more than one transaction — a
/// `touch` and a `sync` commit, then the unmount commits again — and a
/// block written by the first of those is routinely rewritten and freed
/// by the second. It is then still on the disk, still checksums, and is
/// correctly not recorded. The first version of this test asserted over
/// every block newer than the before image and failed on exactly that,
/// which is the kind of wrong a check gets to be once, before it is
/// pointed at a writer that cannot answer back.
#[test]
fn every_block_the_last_commit_wrote_is_recorded_as_allocated() {
    let pairs = pairs();
    if pairs.is_empty() {
        eprintln!("no fixtures; build them with ./scripts/vm-build-cow-fixtures.sh");
        return;
    }

    let mut transactions = 0usize;
    for (what, before, after) in &pairs {
        let fs_before = mount(before);
        let fs_after = mount(after);
        let old_gen = fs_before.superblock().generation;
        let now = fs_after.superblock().generation;
        if now == old_gen {
            eprintln!("{what}: no commit happened on this run — nothing to check");
            continue;
        }
        transactions += 1;
        let recorded = recorded_blocks(&fs_after);

        // Blocks stamped with the CURRENT generation: what the last
        // transaction wrote, and all of which must still be live.
        let written: Vec<(u64, u64, u64)> = present_blocks(&fs_after, after)
            .into_iter()
            .filter(|(_, gen, _)| *gen == now)
            .collect();

        assert!(
            !written.is_empty(),
            "{what}: no block carries generation {now}, so the superblock names a \
             transaction that wrote nothing"
        );

        for (bytenr, gen, owner) in &written {
            assert!(
                recorded.contains(bytenr),
                "{what}: the block at {bytenr} (generation {gen}, tree {owner}) was \
                 written but has no METADATA_ITEM. The allocator will hand that address \
                 out again."
            );
        }
        eprintln!("{what}: {} blocks written, all recorded", written.len());
    }

    assert!(
        transactions > 0,
        "not one pair contained a transaction, so nothing was checked. The fixture \
         builder is producing images that never commit."
    );
}

/// The superblock's total is the sum of the block groups, after as
/// before.
///
/// Checked on both sides of every pair rather than only the result: a
/// transaction that corrupts this leaves a filesystem whose own two
/// accounts of its size disagree.
#[test]
fn usage_adds_up_on_both_sides_of_every_transaction() {
    let pairs = pairs();
    if pairs.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }

    let mut checked = 0usize;
    for (what, before, after) in &pairs {
        for (side, path) in [("before", before), ("after", after)] {
            let fs = mount(path);
            let groups = fs.block_groups().expect("reading block groups");
            let total: u64 = groups.iter().map(|g| g.used).sum();
            assert_eq!(
                total,
                fs.superblock().bytes_used,
                "{what} ({side}): the superblock says {} bytes are used and the {} block \
                 groups sum to {total}",
                fs.superblock().bytes_used,
                groups.len()
            );
            checked += 1;
        }
    }
    eprintln!("{checked} images account for themselves");
}

/// What `bytes_used` moved by is what the allocation records moved by.
///
/// The general form of an observation that looks like a coincidence on
/// these fixtures — `bytes_used` did not change across either
/// transaction — because four blocks were allocated and four released.
/// Stated as a delta it still holds when a tree grows, which is the case
/// the fixtures do not cover and a planner will eventually produce.
#[test]
fn the_change_in_usage_is_the_change_in_recorded_blocks() {
    let pairs = pairs();
    if pairs.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }

    for (what, before, after) in &pairs {
        let fs_before = mount(before);
        let fs_after = mount(after);
        let nodesize = fs_after.superblock().nodesize as i128;

        let was = recorded_blocks(&fs_before);
        let now = recorded_blocks(&fs_after);
        let added = now.difference(&was).count() as i128;
        let removed = was.difference(&now).count() as i128;

        if !committed(before, after) {
            eprintln!("{what}: no commit happened on this run — nothing to check");
            continue;
        }
        let used_delta =
            fs_after.superblock().bytes_used as i128 - fs_before.superblock().bytes_used as i128;
        let record_delta = (added - removed) * nodesize;

        assert_eq!(
            used_delta, record_delta,
            "{what}: bytes_used moved by {used_delta} but the allocation records moved by \
             {record_delta} ({added} added, {removed} removed, nodesize {nodesize}). \
             Those are two accounts of the same thing."
        );
        eprintln!("{what}: {added} recorded, {removed} released, bytes_used {used_delta:+}");
    }
}

/// Every block recorded as allocated is really there.
///
/// The converse of the above, and the more dangerous direction. A
/// `METADATA_ITEM` naming an address nothing was written to is a
/// filesystem that believes a tree block exists where there is
/// whatever was there before. It will not be caught by mounting, or by
/// reading the files — only by descending into that particular block,
/// which may be years later.
///
/// A writer gets this wrong by recording an allocation and then failing
/// to write the block, or by writing it somewhere else.
#[test]
fn every_block_recorded_as_allocated_is_really_on_the_disk() {
    let pairs = pairs();
    if pairs.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }

    let mut checked = 0usize;
    for (what, before, after) in &pairs {
        for (side, path) in [("before", before), ("after", after)] {
            let fs = mount(path);
            let present: BTreeSet<u64> = present_blocks(&fs, path)
                .into_iter()
                .map(|(bytenr, _, _)| bytenr)
                .collect();

            for bytenr in recorded_blocks(&fs) {
                assert!(
                    present.contains(&bytenr),
                    "{what} ({side}): a METADATA_ITEM names {bytenr}, but no tree block \
                     with that address and a valid checksum is on the disk. The \
                     filesystem believes there is a tree block where there is not."
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "no recorded block was checked");
    eprintln!("{checked} recorded blocks are all physically present");
}

/// The accounting check is not vacuous.
///
/// Every assertion above passes on every fixture, which is what it looks
/// like when a check is right and also what it looks like when a check
/// cannot fail. The kernel's images cannot be made to violate an
/// invariant on request, so one is broken deliberately here: a copy of a
/// fixture with `bytes_used` moved by one block, re-checksummed so it
/// still mounts.
///
/// If this does not fail, the sum check is decoration.
#[test]
fn an_image_whose_accounts_disagree_is_caught() {
    let Some(path) = image("btrfs-cow-before.img") else {
        eprintln!("no fixtures — skipping");
        return;
    };

    let mut bytes = std::fs::read(&path).expect("reading a fixture");
    let sb_at = fs_btrfs::superblock::SUPER_OFFSETS[0] as usize;
    let mut raw = bytes[sb_at..sb_at + 4096].to_vec();

    let nodesize = {
        let fs = mount(&path);
        fs.superblock().nodesize as u64
    };

    // One block's worth of usage that no block group accounts for.
    let at = fs_btrfs::super_write::offsets::BYTES_USED;
    let was = le64(&raw, at);
    raw[at..at + 8].copy_from_slice(&(was + nodesize).to_le_bytes());
    // Re-checksummed, or it would not mount and this would prove
    // nothing about the check under test.
    fs_btrfs::super_write::stamp_checksum(&mut raw, fs_btrfs::superblock::ChecksumType::Crc32c);
    bytes[sb_at..sb_at + 4096].copy_from_slice(&raw);

    let broken = std::env::temp_dir().join("btrfs-cow-accounts-disagree.img");
    std::fs::write(&broken, &bytes).expect("writing the broken copy");

    let fs = mount(&broken);
    assert_eq!(
        fs.superblock().bytes_used,
        was + nodesize,
        "the edit did not take, so nothing was tested"
    );

    let groups = fs.block_groups().expect("reading block groups");
    let total: u64 = groups.iter().map(|g| g.used).sum();
    assert_ne!(
        total,
        fs.superblock().bytes_used,
        "an image edited to disagree with itself still adds up, so the check that \
         compares them cannot fail"
    );

    let _ = std::fs::remove_file(&broken);
    eprintln!(
        "a superblock claiming {} against block groups summing to {total} is detectable",
        fs.superblock().bytes_used
    );
}
