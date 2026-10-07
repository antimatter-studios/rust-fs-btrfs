//! A pool with members left out mounts degraded and reads every file the
//! kernel wrote, when its redundancy covers the loss (#300).
//!
//! The fixtures are made by the kernel in the harness VM (`chore
//! fixtures`): RAID5 over three devices and RAID6 over four, data AND
//! metadata; the two-device RAID1 pool; and a two-device RAID0 pool with
//! no redundancy at all. Each manifest gives every file's size and
//! SHA-256 as the kernel read it while mounted.
//!
//! # What each test is a check on
//!
//! - **A missing member is a lost element, not a damaged one.** Its
//!   bytes are not there to be read and fail a checksum; every element
//!   on it has to be read the other way its chunk offers, a parity
//!   rebuild or the other copy, from the first read on — the chunk tree
//!   itself included, which lives in the same profile.
//! - **Every subset the profile tolerates.** RAID5 without each member in
//!   turn, RAID6 without every pair, so every rebuild (from P, from Q,
//!   from both) is used against the kernel's bytes.
//! - **A loss the profile does not cover is refused at mount**, naming
//!   the chunk, rather than mounting and failing on the first read that
//!   lands on a lost element: RAID0, and RAID5 without two members.
//! - **Degraded is a choice.** `mount_pool` still refuses an incomplete
//!   set, so a caller that did not ask for a degraded mount never gets one.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{fixture, sha256_hex};
use fs_core::{BlockRead, FileDevice};
use std::path::PathBuf;
use std::sync::Arc;

/// The members of a pool fixture, in devid order.
fn members(name: &str, count: usize) -> Vec<PathBuf> {
    if name == "pool" {
        return vec![fixture("btrfs-pool-a.img"), fixture("btrfs-pool-b.img")];
    }
    (1..=count)
        .map(|i| fixture(&format!("btrfs-{name}-{i}.img")))
        .collect()
}

/// `(path, size, sha256)` for every file in the pool's manifest.
fn manifest(name: &str) -> Vec<(String, usize, String)> {
    let path = fixture(&format!("btrfs-{name}.manifest"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let files: Vec<(String, usize, String)> = text
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() == 3 && f[1] != "dir").then(|| {
                (
                    f[0].to_string(),
                    f[1].parse().expect("a size"),
                    f[2].to_string(),
                )
            })
        })
        .collect();
    assert!(
        files.len() >= 4,
        "the {name} manifest names {} files",
        files.len()
    );
    files
}

/// The pool's members with the ones at `left_out` not given.
fn without(name: &str, count: usize, left_out: &[usize]) -> Vec<Arc<dyn BlockRead>> {
    members(name, count)
        .iter()
        .enumerate()
        .filter(|(i, _)| !left_out.contains(i))
        .map(|(_, p)| {
            Arc::new(FileDevice::open(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
                as Arc<dyn BlockRead>
        })
        .collect()
}

/// Mount degraded without `left_out` and require every file to read back
/// as the kernel wrote it.
fn reads_what_the_kernel_wrote(name: &str, count: usize, left_out: &[usize]) {
    let what = format!("{name} without members {left_out:?}");
    let fs = Filesystem::mount_pool_degraded(without(name, count, left_out))
        .unwrap_or_else(|e| panic!("{what}: a degraded mount should succeed: {e}"));
    for (path, size, digest) in manifest(name) {
        let got = fs
            .read_path(&path)
            .unwrap_or_else(|e| panic!("{what}: reading {path}: {e}"));
        assert_eq!(got.len(), size, "{what}: {path}: length");
        assert_eq!(
            sha256_hex(&got),
            digest,
            "{what}: {path}: {size} bytes read, but not the bytes the kernel wrote"
        );
    }
}

/// The error a degraded mount gives, for a loss it must refuse.
fn refusal(name: &str, count: usize, left_out: &[usize]) -> String {
    match Filesystem::mount_pool_degraded(without(name, count, left_out)) {
        Ok(_) => panic!(
            "{name} without members {left_out:?} was mounted, with bytes lost that nothing \
             else holds"
        ),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_raid5_pool_reads_every_file_without_any_one_member() {
    for d in 0..3 {
        reads_what_the_kernel_wrote("raid5", 3, &[d]);
    }
}

#[test]
fn a_raid6_pool_reads_every_file_without_any_two_members() {
    for a in 0..4 {
        reads_what_the_kernel_wrote("raid6", 4, &[a]);
        for b in a + 1..4 {
            reads_what_the_kernel_wrote("raid6", 4, &[a, b]);
        }
    }
}

#[test]
fn a_raid1_pool_reads_every_file_without_either_member() {
    for d in 0..2 {
        reads_what_the_kernel_wrote("pool", 2, &[d]);
    }
}

/// Which chunk the refusal names first depends on where mkfs put it, so
/// the check is that it is a chunk with no redundancy, and says why.
#[test]
fn a_raid0_pool_without_a_member_is_refused_naming_the_profile() {
    for d in 0..2 {
        let msg = refusal("raid0", 2, &[d]);
        assert!(
            (msg.contains("raid0") || msg.contains("single"))
                && msg.contains("cannot be read without them"),
            "the refusal should name a profile with no redundancy, and say why: {msg}"
        );
    }
}

#[test]
fn a_raid5_pool_without_two_members_is_refused() {
    let msg = refusal("raid5", 3, &[0, 1]);
    assert!(
        msg.contains("cannot be read without them"),
        "the refusal should say why: {msg}"
    );
}

#[test]
fn an_incomplete_pool_is_still_refused_unless_degraded_is_asked_for() {
    match Filesystem::mount_pool(without("raid5", 3, &[2])) {
        Ok(_) => panic!("mount_pool accepted a RAID5 pool with a member left out"),
        Err(e) => assert!(e.to_string().contains("spans 3 devices"), "{e}"),
    }
}

/// The whole RAID0 pool still reads, so the refusal above is about the
/// missing member and not about the fixture.
#[test]
fn the_whole_raid0_pool_reads_what_the_kernel_wrote() {
    let fs = Filesystem::mount_pool(without("raid0", 2, &[])).expect("the whole RAID0 pool");
    for (path, size, digest) in manifest("raid0") {
        let got = fs
            .read_path(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(got.len(), size, "{path}: length");
        assert_eq!(sha256_hex(&got), digest, "{path}: contents");
    }
}
