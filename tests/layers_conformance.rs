//! rust-fs-core's conformance checks for the three layers, over this
//! driver's kernel-made fixtures and damaged copies of them.
//!
//! The checks hold the driver to the contract's shape: bounds refused,
//! flags and refusals agreeing, the layers agreeing about the same bytes,
//! every reachable file reading back its length, and every candidate's walk
//! rebuilding the file's bytes. Whether those bytes are the right ones is
//! `layers_oracle.rs`'s, against btrfs-progs.

use fs_btrfs::layers::BtrfsVolume;
use fs_btrfs_test_support::{copy_sparse, fixture, temp_path};
use fs_core::layers::conformance::{self, Limits};
use fs_core::layers::OpenVolume;
use fs_core::FileDevice;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

fn limits() -> Limits {
    Limits::default()
}

const HEALTHY: &[&str] = &[
    "btrfs-rich.img",
    "btrfs-comp-zlib.img",
    "btrfs-comp-lzo.img",
    "btrfs-comp-zstd.img",
    "btrfs-subvol.img",
    "btrfs-dup.img",
    "btrfs-mixed.img",
    "btrfs-node16k.img",
    "btrfs-csum-sha256.img",
    "btrfs-nodatacow.img",
];

/// Every healthy fixture passes the volume, primitive and file checks, and
/// its candidates' walks rebuild its files.
#[test]
fn healthy_fixtures_conform() {
    let mut failed = Vec::new();
    for name in HEALTHY {
        let image = fixture(name);
        let volume = BtrfsVolume::open(FileDevice::open(&image).unwrap())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut report = conformance::check_healthy(&volume, &[], &limits());
        report.merge(conformance::check_files_against_candidates(
            &volume,
            &limits(),
        ));
        for check in report.failures() {
            failed.push(format!("{name}: {} {}", check.name, check.detail));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

fn scratch(name: &str, tag: &str) -> PathBuf {
    let dir = PathBuf::from(temp_path!("layers-conformance-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join(name);
    copy_sparse(fixture(name), &image).unwrap();
    image
}

/// The copy of the filesystem tree's root block that `mirror` names.
fn fs_root_physical(image: &Path) -> (Vec<u64>, u64) {
    use fs_btrfs::salvage::{
        best_copy, build_chunk_map, read_root_items, root_tree_candidates, superblock_copies,
        Layout,
    };
    let device = FileDevice::open(image).unwrap();
    let copies = superblock_copies(&device);
    let sb = best_copy(&copies).unwrap().superblock.clone().unwrap();
    let layout = Layout::new(&sb, build_chunk_map(&device, &sb).map);
    let (roots, _) = read_root_items(&device, &layout, root_tree_candidates(&copies)[0].tree_root);
    let fs = roots.iter().find(|r| r.objectid == 5).unwrap();
    let physical = (0..layout.copies(fs.bytenr))
        .map(|m| layout.chunks.map_mirror(fs.bytenr, m).unwrap().physical)
        .collect();
    (physical, u64::from(sb.nodesize))
}

/// Damaged copies open for salvage and pass the damaged checks: the
/// primary superblock's checksum broken, the filesystem tree's root block
/// broken in every copy, and the image cut at 64 MiB.
#[test]
fn damaged_copies_conform() {
    let primary = scratch("btrfs-comp-zstd.img", "primary");
    let file = OpenOptions::new().write(true).open(&primary).unwrap();
    file.write_all_at(b"X", 0x1_0000 + 0x12b).unwrap();

    let leaf = scratch("btrfs-comp-zstd.img", "fs-root");
    let (copies, nodesize) = fs_root_physical(&leaf);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&leaf)
        .unwrap();
    for at in copies {
        let mut byte = [0u8];
        file.read_exact_at(&mut byte, at + nodesize - 1).unwrap();
        file.write_all_at(&[byte[0] ^ 0xff], at + nodesize - 1)
            .unwrap();
    }

    let truncated = scratch("btrfs-comp-zstd.img", "truncated");
    OpenOptions::new()
        .write(true)
        .open(&truncated)
        .unwrap()
        .set_len(64 << 20)
        .unwrap();

    for image in [primary, leaf, truncated] {
        let volume = BtrfsVolume::open_for_salvage(FileDevice::open(&image).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", image.display()));
        let report = conformance::check_damaged(&volume, &limits());
        assert!(report.passed(), "{}:\n{report}", image.display());
    }
}
