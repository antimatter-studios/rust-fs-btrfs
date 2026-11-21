//! Salvage checks file data against the checksum tree as btrfs check
//! does, and places every extent on the device where btrfs-progs does.
//!
//! In the harness VM: `btrfs check --check-data-csum` reads every data
//! sector against its checksum and names each that fails by logical
//! address; `btrfs-map-logical` names where a logical extent lies on the
//! device. On kernel-made volumes every file's data has to verify, and on
//! copies with a data sector damaged, salvage has to name the same sectors
//! btrfs check names, and nothing else.

use fs_btrfs::salvage::{
    best_copy, build_chunk_map, collect_inodes, copy_file, read_root_items, root_tree_candidates,
    superblock_copies, CsumTree, DataFault, ExtentType, InodeScan, Layout,
};
use fs_btrfs_test_support::{copy_sparse, fixture, oracle, temp_path};
use fs_core::FileDevice;
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;

struct Volume {
    device: FileDevice,
    layout: Layout,
    scan: InodeScan,
    csums: Option<CsumTree>,
}

fn open(image: &Path) -> Volume {
    let device = FileDevice::open(image).unwrap();
    let copies = superblock_copies(&device);
    let sb = best_copy(&copies).unwrap().superblock.clone().unwrap();
    let layout = Layout::new(&sb, build_chunk_map(&device, &sb).map);
    let (roots, _) = read_root_items(&device, &layout, root_tree_candidates(&copies)[0].tree_root);
    let fs = roots.iter().find(|r| r.objectid == 5).unwrap().pointer();
    let csums = roots
        .iter()
        .find(|r| r.objectid == 7)
        .map(|r| CsumTree::new(r.pointer()));
    let scan = collect_inodes(&device, &layout, fs);
    Volume {
        device,
        layout,
        scan,
        csums,
    }
}

/// The regular files' inode numbers, with what copying each reported.
fn copy_all(volume: &Volume) -> Vec<(u64, fs_btrfs::salvage::FileCopy)> {
    volume
        .scan
        .inodes
        .values()
        .filter(|r| r.inode.as_ref().is_some_and(|i| i.mode & S_IFMT == S_IFREG))
        .map(|record| {
            let mut sink = Vec::new();
            let report = copy_file(
                &volume.device,
                &volume.layout,
                volume.csums.as_ref(),
                record,
                &mut sink,
            )
            .unwrap();
            (record.ino, report)
        })
        .collect()
}

/// The logical addresses `btrfs check --check-data-csum` names, and
/// whether it passed.
fn check_data(image: &Path) -> (bool, BTreeSet<u64>, String) {
    let out = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(image)
        .output();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let named = text
        .lines()
        .filter(|line| line.contains("csum") && line.contains("bytenr"))
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            while let Some(word) = words.next() {
                if word == "bytenr" {
                    return words.next()?.trim_end_matches(',').parse().ok();
                }
            }
            None
        })
        .collect();
    (out.status.success(), named, text)
}

/// Every file of kernel-made volumes with checksummed data, compressed and
/// not, verifies sector by sector, as btrfs check says it does; on a
/// nodatacow volume's nodatacow files nothing is checked, and nothing fails.
#[test]
fn healthy_data_verifies_as_btrfs_check_says() {
    for name in [
        "btrfs-rich.img",
        "btrfs-comp-zlib.img",
        "btrfs-comp-lzo.img",
        "btrfs-comp-zstd.img",
    ] {
        let image = fixture(name);
        let (ok, named, text) = check_data(&image);
        assert!(ok && named.is_empty(), "{name}: btrfs check:\n{text}");
        let volume = open(&image);
        assert!(volume.csums.is_some(), "{name} has a checksum tree");
        let copies = copy_all(&volume);
        let verified: u64 = copies.iter().map(|(_, c)| c.sectors.verified).sum();
        assert!(verified > 0, "{name}: sectors were checked");
        for (ino, copy) in &copies {
            assert!(
                copy.faults.is_empty(),
                "{name} inode {ino}: {:?}",
                copy.faults
            );
            assert_eq!(copy.sectors.failed, 0, "{name} inode {ino}");
        }
    }

    let volume = open(&fixture("btrfs-nodatacow.img"));
    let copies = copy_all(&volume);
    assert!(copies.iter().any(|(_, c)| c.sectors.unchecked > 0));
    assert!(copies
        .iter()
        .all(|(_, c)| c.sectors.failed == 0 && c.faults.is_empty()));
}

/// A scratch copy of `name` with one byte of `pick`'s on-disk data flipped,
/// and the logical sector it landed in.
fn damaged_data(name: &str, tag: &str, compressed: bool) -> (PathBuf, u64) {
    let dir = PathBuf::from(temp_path!("salvage-data-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join(name);
    copy_sparse(fixture(name), &image).unwrap();
    let volume = open(&image);
    let extent = volume
        .scan
        .inodes
        .values()
        .flat_map(|r| r.extents.iter())
        .find(|e| {
            e.item.kind == ExtentType::Regular
                && e.item.disk_bytenr != 0
                && (e.item.compression != 0) == compressed
                && e.item.disk_num_bytes >= 4096
        })
        .expect("a data extent of the kind wanted");
    let sector = u64::from(volume.layout.sectorsize);
    let logical = extent.item.disk_bytenr;
    assert!(extent.item.disk_num_bytes >= sector);
    let at = volume.layout.chunks.map(logical).unwrap().physical + 100;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image)
        .unwrap();
    let mut byte = [0u8];
    file.read_exact_at(&mut byte, at).unwrap();
    file.write_all_at(&[byte[0] ^ 0xff], at).unwrap();
    (image, logical)
}

/// One flipped byte in an uncompressed and in a compressed extent: btrfs
/// check names that sector, and salvage names the same one, keeps the
/// other files clean, and still writes every file out.
#[test]
fn a_damaged_data_sector_is_named_as_btrfs_check_names_it() {
    for (name, compressed) in [
        ("btrfs-comp-zstd.img", false),
        ("btrfs-comp-zstd.img", true),
    ] {
        let (image, logical) =
            damaged_data(name, if compressed { "zstd" } else { "plain" }, compressed);
        let (ok, named, text) = check_data(&image);
        assert!(!ok, "{name}: btrfs check passed damaged data:\n{text}");
        assert_eq!(named, BTreeSet::from([logical]), "{name}:\n{text}");

        let volume = open(&image);
        let copies = copy_all(&volume);
        let failed: BTreeSet<u64> = copies
            .iter()
            .flat_map(|(_, c)| c.faults.iter())
            .filter_map(|(_, fault)| match fault {
                DataFault::Checksum { logical, .. } => Some(*logical),
                _ => None,
            })
            .collect();
        assert_eq!(failed, named, "{name}");
        let failing_files = copies.iter().filter(|(_, c)| c.sectors.failed > 0).count();
        assert_eq!(failing_files, 1, "{name}: only the damaged file fails");
    }
}

/// Every regular data extent lies on the device where btrfs-map-logical
/// says, and every inline extent's stored bytes lie inside its leaf where
/// the file's content is.
#[test]
fn extents_lie_where_btrfs_map_logical_says() {
    let image = fixture("btrfs-rich.img");
    let volume = open(&image);
    let mut regular = 0;
    let mut inline = 0;
    for record in volume.scan.inodes.values() {
        for claim in &record.extents {
            let item = &claim.item;
            if let Some((logical, length)) = item.logical_range() {
                let ours = volume.layout.physical_runs(logical, length, 0).unwrap();
                let out = oracle("btrfs-map-logical")
                    .args(["-l", &logical.to_string(), "-b", &length.to_string()])
                    .arg(&image)
                    .output();
                let text = String::from_utf8_lossy(&out.stdout);
                let first: u64 = text
                    .lines()
                    .find(|l| l.starts_with("mirror 1"))
                    .and_then(|l| l.split_whitespace().skip_while(|w| *w != "physical").nth(1))
                    .and_then(|w| w.parse().ok())
                    .unwrap_or_else(|| panic!("btrfs-map-logical -l {logical}:\n{text}"));
                assert_eq!(ours[0].0, first, "extent at {logical}");
                regular += 1;
            }
            if let Some(logical) = claim.inline_logical() {
                let runs = volume
                    .layout
                    .physical_runs(logical, item.inline.len() as u64, claim.leaf_mirror)
                    .unwrap();
                assert_eq!(runs.len(), 1, "inline bytes lie inside one block");
                let mut stored = vec![0u8; item.inline.len()];
                volume.device_read(runs[0].0, &mut stored);
                assert_eq!(stored, item.inline, "inode {} inline bytes", record.ino);
                inline += 1;
            }
        }
    }
    assert!(
        regular > 2 && inline > 0,
        "{regular} regular, {inline} inline"
    );
}

impl Volume {
    fn device_read(&self, offset: u64, buf: &mut [u8]) {
        use fs_core::BlockRead;
        self.device.read_at(offset, buf).unwrap();
    }
}
