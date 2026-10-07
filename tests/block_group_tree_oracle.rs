//! A volume with the block group tree reads, and its block groups are
//! the ones btrfs-progs finds there (#270).
//!
//! With `-O block-group-tree` the block group items leave the extent tree
//! for a tree of their own, objectid 11. A reader that still looks in the
//! extent tree finds no block group at all: the listing is empty and
//! `fsck.btrfs` reports every byte the superblock counts as used as
//! belonging to no group, on a volume `btrfs check` finds clean.
//!
//! The fixture is `features/btrfs-bgt.img`, made and populated by the
//! kernel in the harness VM; its manifest is every file's size and
//! SHA-256 as the kernel read it, and the builder refuses to publish it
//! unless `btrfs check` finds it clean and the superblock carries the
//! feature.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{dump_tree, fixture, sha256_hex};
use fs_core::FileDevice;
use std::sync::Arc;

fn image() -> std::path::PathBuf {
    fixture("features/btrfs-bgt.img")
}

fn mount() -> Filesystem {
    let dev = FileDevice::open(image()).expect("open features/btrfs-bgt.img");
    Filesystem::mount(Arc::new(dev)).expect("mount the block group tree volume")
}

/// `(start, length, used)` for every `BLOCK_GROUP_ITEM` in
/// `btrfs inspect-internal dump-tree -t 11`, which prints them as
///
/// ```text
/// item 0 key (13631488 BLOCK_GROUP_ITEM 8388608) itemoff 16259 itemsize 24
///         block group used 16384 chunk_objectid 256 flags DATA
/// ```
fn reference() -> Vec<(u64, u64, u64)> {
    let dump = dump_tree(&image(), "11");
    let mut out = Vec::new();
    let mut key: Option<(u64, u64)> = None;
    for line in dump.lines().map(str::trim) {
        if let Some(rest) = line.split("key (").nth(1) {
            let f: Vec<&str> = rest.split([' ', ')']).collect();
            key = (f.get(1) == Some(&"BLOCK_GROUP_ITEM"))
                .then(|| (f[0].parse().expect("start"), f[2].parse().expect("length")));
        } else if let Some(rest) = line.strip_prefix("block group used ") {
            let (start, length) = key.take().expect("a used line follows its key");
            let used = rest.split(' ').next().unwrap().parse().expect("used");
            out.push((start, length, used));
        }
    }
    assert!(
        out.len() >= 3,
        "btrfs-progs lists {} block groups in tree 11; a fresh volume has data, metadata \
         and system",
        out.len()
    );
    out
}

#[test]
fn the_block_groups_are_the_ones_btrfs_progs_lists() {
    let got: Vec<(u64, u64, u64)> = mount()
        .block_groups()
        .expect("block groups")
        .iter()
        .map(|g| (g.start, g.length, g.used))
        .collect();
    assert_eq!(got, reference());
}

#[test]
fn fsck_finds_the_volume_clean_as_btrfs_check_does() {
    let report = fs_btrfs::check::check(&mount());
    let findings: Vec<String> = report.findings.iter().map(|f| f.what.clone()).collect();
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn every_file_reads_what_the_kernel_wrote() {
    let fs = mount();
    let manifest =
        std::fs::read_to_string(fixture("features/btrfs-bgt.manifest")).expect("manifest");
    let mut checked = 0;
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 3, "a manifest line: {line:?}");
        let got = fs
            .read_path(f[0])
            .unwrap_or_else(|e| panic!("{}: {e}", f[0]));
        assert_eq!(got.len().to_string(), f[1], "{}: length", f[0]);
        assert_eq!(sha256_hex(&got), f[2], "{}: contents", f[0]);
        checked += 1;
    }
    assert!(checked >= 4, "the manifest names {checked} files");
}
