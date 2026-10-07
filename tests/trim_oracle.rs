//! A trim discards exactly the free ranges btrfs-progs lists, on every
//! copy, where the chunk tree btrfs-progs prints says each copy is (#305).
//!
//! # The oracles
//!
//! What is free is the free-space tree's, and the free-space tree is the
//! kernel's: the fixtures are made by `mkfs.btrfs` and filled by the
//! kernel in the harness VM. `btrfs inspect-internal dump-tree -t 10`
//! lists its `FREE_SPACE_EXTENT` items, which are the runs a trim must
//! discard — no more, which would throw away allocated bytes, and no
//! fewer, which would leave the device believing free space is in use.
//!
//! Where each copy of a run is on the device is the chunk tree's answer as
//! btrfs-progs prints it: on a DUP volume, whose two copies sit at two
//! different offsets of the one device, copy `k` of a run is stripe `k`'s
//! offset plus the run's offset into its chunk.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::trim::TrimRange;
use fs_btrfs_test_support::{dump_tree, fixture, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Fixtures with a free-space tree and different profiles: single data
/// with DUP metadata, and DUP throughout. Not the populated fixtures: a
/// fragmented group's free space may be kept as a bitmap, whose bits
/// dump-tree does not print, and then there is nothing to compare with.
const IMAGES: [&str; 2] = ["btrfs-default.img", "btrfs-dup.img"];

fn mount(image: &Path) -> Filesystem {
    let dev = FileDevice::open(image).unwrap_or_else(|e| panic!("{}: {e}", image.display()));
    Filesystem::mount(Arc::new(dev)).unwrap_or_else(|e| panic!("{}: {e}", image.display()))
}

/// Adjacent runs as one, which is how both sides are compared: the tree
/// may record one run as two items that abut.
fn merged(mut runs: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    runs.sort();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (start, len) in runs {
        match out.last_mut() {
            Some((s, l)) if *s + *l == start => *l += len,
            _ => out.push((start, len)),
        }
    }
    out
}

/// `(start, length)` of every `FREE_SPACE_EXTENT` btrfs-progs lists in
/// the free-space tree, which it prints as
///
/// ```text
/// item 1 key (13631488 FREE_SPACE_EXTENT 8388608) itemoff 16242 itemsize 0
/// ```
fn progs_free(image: &Path) -> Vec<(u64, u64)> {
    let dump = dump_tree(image, "10");
    assert!(
        !dump.contains("FREE_SPACE_BITMAP"),
        "{}: the free-space tree holds bitmaps, whose bits dump-tree does not print; this \
         fixture cannot be the reference",
        image.display()
    );
    let runs: Vec<(u64, u64)> = dump
        .lines()
        .filter_map(|line| {
            let rest = line.split("key (").nth(1)?;
            let f: Vec<&str> = rest.split([' ', ')']).collect();
            (f.get(1) == Some(&"FREE_SPACE_EXTENT"))
                .then(|| (f[0].parse().expect("start"), f[2].parse().expect("length")))
        })
        .collect();
    assert!(
        !runs.is_empty(),
        "{}: btrfs-progs lists no free run at all",
        image.display()
    );
    merged(runs)
}

/// The first copy of every range, as `(logical, length)`.
fn first_copies(ranges: &[TrimRange]) -> Vec<(u64, u64)> {
    merged(
        ranges
            .iter()
            .filter(|r| r.mirror == 0)
            .map(|r| (r.logical, r.len))
            .collect(),
    )
}

#[test]
fn the_runs_trimmed_are_the_free_runs_btrfs_progs_lists() {
    for name in IMAGES {
        let image = fixture(name);
        let ranges = mount(&image).trim_ranges().expect("trim ranges");
        assert_eq!(
            first_copies(&ranges),
            progs_free(&image),
            "{name}: the runs a trim discards are not the free-space tree's"
        );
    }
}

/// A chunk as btrfs-progs prints it: `(logical, length, [(devid, offset)])`.
type ProgsChunk = (u64, u64, Vec<(u64, u64)>);

/// Every chunk btrfs-progs lists in the chunk tree, which it prints as
///
/// ```text
/// item 2 key (FIRST_CHUNK_TREE CHUNK_ITEM 22020096) itemoff 15975 itemsize 112
///         length 8388608 owner 2 stripe_len 65536 type SYSTEM|DUP
///         ...
///                 stripe 0 devid 1 offset 22020096
///                 stripe 1 devid 1 offset 30408704
/// ```
fn progs_chunks(image: &Path) -> Vec<ProgsChunk> {
    let dump = dump_tree(image, "3");
    let mut out: Vec<ProgsChunk> = Vec::new();
    for line in dump.lines().map(str::trim) {
        if let Some(rest) = line.split("CHUNK_ITEM ").nth(1) {
            let logical = rest
                .split(')')
                .next()
                .unwrap()
                .parse()
                .expect("chunk logical");
            out.push((logical, 0, Vec::new()));
        } else if let Some(rest) = line.strip_prefix("length ") {
            if let Some(chunk) = out.last_mut() {
                chunk.1 = rest
                    .split(' ')
                    .next()
                    .unwrap()
                    .parse()
                    .expect("chunk length");
            }
        } else if line.starts_with("stripe ") {
            let f: Vec<&str> = line.split_whitespace().collect();
            if let (Some(chunk), Some(&"devid"), Some(&"offset")) =
                (out.last_mut(), f.get(2), f.get(4))
            {
                chunk
                    .2
                    .push((f[3].parse().expect("devid"), f[5].parse().expect("offset")));
            }
        }
    }
    assert!(!out.is_empty(), "btrfs-progs lists no chunk");
    out
}

/// On a DUP volume each copy of a free run is at the same offset into its
/// chunk's stripe, and the stripes are where btrfs-progs says: copy `k`
/// of `logical` is at stripe `k`'s offset plus `logical - chunk start`.
#[test]
fn every_copy_is_discarded_where_the_chunk_tree_puts_it() {
    let image = fixture("btrfs-dup.img");
    let ranges = mount(&image).trim_ranges().expect("trim ranges");
    let chunks = progs_chunks(&image);
    let mut copies = 0;
    for r in &ranges {
        let (start, _, stripes) = chunks
            .iter()
            .find(|(start, len, _)| r.logical >= *start && r.logical < start + len)
            .unwrap_or_else(|| panic!("{r:?}: no chunk btrfs-progs lists holds it"));
        assert_eq!(stripes.len(), 2, "{r:?}: a DUP chunk has two stripes");
        let (devid, offset) = stripes[r.mirror];
        assert_eq!(
            (r.devid, r.physical),
            (devid, offset + (r.logical - start)),
            "{r:?}: discarded where btrfs-progs does not put copy {}",
            r.mirror
        );
        copies += 1;
    }
    let runs = first_copies(&ranges).len();
    assert!(
        copies >= 2 * runs && runs > 0,
        "{copies} ranges for {runs} free runs: a DUP run has two copies"
    );
}

/// A scratch copy, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn a_trim_hands_every_range_to_the_discard_and_needs_a_writable_mount() {
    let image = fixture("btrfs-dup.img");
    let ro = mount(&image);
    assert!(matches!(
        ro.trim(&mut |_| Ok(())),
        Err(fs_btrfs::error::Error::ReadOnly)
    ));
    let want = ro.trim_ranges().expect("trim ranges");

    let copy = Scratch(PathBuf::from(temp_path!(
        "btrfs-trim-{}-{:?}.img",
        std::process::id(),
        std::thread::current().id()
    )));
    std::fs::copy(&image, &copy.0).expect("copy the fixture");
    let dev = Arc::new(FileDevice::open_rw(&copy.0).expect("open read-write"));
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write");
    let mut discarded = Vec::new();
    let report = fs
        .trim(&mut |r| {
            discarded.push(*r);
            Ok(())
        })
        .expect("trim");
    assert_eq!(discarded, want, "every range, once, in order");
    assert_eq!(report.ranges, want);
    assert_eq!(report.bytes, want.iter().map(|r| r.len).sum::<u64>());
    assert!(report.bytes > 0, "a fresh DUP volume has free space");
}
