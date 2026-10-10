//! A read-only scrub finds the copies the kernel's scrub finds, and only
//! those (#268).
//!
//! The fixture is `scrub/btrfs-scrub.img`: a DUP volume the kernel
//! populated, then three copies damaged on purpose — the second copy of a
//! data sector, the first copy of the next one, and the second copy of
//! the fs tree's root block — and then scrubbed read-only by the kernel.
//! Its manifest records the damaged addresses and the kernel's counters.
//!
//! Two things are checked. That every copy the fixture damaged is
//! reported, at its address and as the copy it is; and that the total
//! matches the kernel's `read_errors + csum_errors + verify_errors`, so
//! nothing undamaged is reported either. Every damaged copy has a good
//! twin, so every error is repairable.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::scrub::ScrubTarget;
use fs_btrfs_test_support::fixture;
use fs_core::FileDevice;
use std::collections::BTreeSet;
use std::sync::Arc;

fn manifest() -> String {
    std::fs::read_to_string(fixture("scrub/btrfs-scrub.manifest")).expect("scrub manifest")
}

/// `(target, logical, copy from 0)` for every `damaged` line.
fn damaged() -> BTreeSet<(ScrubTarget, u64, usize)> {
    let out: BTreeSet<_> = manifest()
        .lines()
        .filter_map(|l| l.strip_prefix("damaged "))
        .map(|rest| {
            let f: Vec<&str> = rest.split_whitespace().collect();
            let what = match f[0] {
                "data" => ScrubTarget::Data,
                "tree" => ScrubTarget::TreeBlock,
                other => panic!("a damaged line naming {other:?}"),
            };
            let copy: usize = f[2].parse().expect("copy");
            (what, f[1].parse().expect("logical"), copy - 1)
        })
        .collect();
    assert_eq!(out.len(), 3, "the fixture damages three copies");
    out
}

/// A counter from the kernel's `btrfs scrub start -R` output.
fn kernel_counter(name: &str) -> u64 {
    manifest()
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("the kernel's scrub printed no {name}"))
        .trim()
        .parse()
        .expect("a count")
}

fn scrub() -> fs_btrfs::scrub::ScrubReport {
    let dev = FileDevice::open(fixture("scrub/btrfs-scrub.img")).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("a damaged second copy still mounts");
    fs.scrub().expect("scrub")
}

#[test]
fn every_damaged_copy_is_found_where_it_was_damaged() {
    let report = scrub();
    let found: BTreeSet<_> = report
        .errors
        .iter()
        .map(|e| (e.what, e.logical, e.mirror))
        .collect();
    assert_eq!(found, damaged(), "{:#?}", report.errors);
    assert!(
        report.errors.iter().all(|e| e.repairable),
        "every damaged copy has a good twin: {:#?}",
        report.errors
    );
}

/// The kernel's counters say how many bad copies it found, and no more
/// are reported here.
///
/// The kernel counts in SECTORS since its scrub was rewritten (6.4): a
/// bad tree block counts once per sector it spans. A kernel counting
/// blocks gives the smaller figure. Either way the count is fixed by what
/// was found, and an extra or a missing copy here changes both figures.
#[test]
fn as_many_errors_as_the_kernel_counted() {
    let kernel = kernel_counter("read_errors")
        + kernel_counter("csum_errors")
        + kernel_counter("verify_errors");
    let report = scrub();
    let dev = FileDevice::open(fixture("scrub/btrfs-scrub.img")).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let per_block = u64::from(fs.superblock().nodesize / fs.superblock().sectorsize);
    let blocks = report.errors.len() as u64;
    let sectors: u64 = report
        .errors
        .iter()
        .map(|e| match e.what {
            ScrubTarget::TreeBlock => per_block,
            ScrubTarget::Data | ScrubTarget::Parity => 1,
        })
        .sum();
    assert!(
        kernel == sectors || kernel == blocks,
        "the kernel counted {kernel}; this found {blocks} copies, {sectors} sectors: {:#?}",
        report.errors
    );
    assert!(report.tree_blocks > 0 && report.data_bytes > 0);
}
