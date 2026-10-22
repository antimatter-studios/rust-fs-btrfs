//! An incompatible feature this driver cannot read is refused by name
//! (#270, #447, #448).
//!
//! The refusal itself was always there: `Superblock::parse` refuses any
//! incompat bit outside `incompat::SUPPORTED`. What it said was
//! `incompatible feature bits 0x0000000000002000 not implemented`, which
//! tells a user nothing they can act on. A feature this driver knows and
//! deliberately does not read is named; a bit it does not know is still
//! given by its value.
//!
//! The images are fresh `mkfs.btrfs` files with one bit OR-ed into the
//! primary superblock's `incompat_flags`. Neither the guest kernel nor its
//! btrfs-progs can make an extent-tree-v2 volume -- both accept the
//! feature only in experimental builds -- and the refusal happens at the
//! superblock, before any tree the feature restructures is read. So
//! setting the bit on a volume btrfs-progs made is the case that matters.
//! `mkfs.btrfs` runs in the harness VM, so nothing here skips.

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::stamp_checksum;
use fs_btrfs::superblock::{incompat, offsets, ChecksumType};
use fs_btrfs_test_support::{oracle, temp_path};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::os::unix::fs::FileExt;
use std::sync::Arc;

const SUPERBLOCK: u64 = 0x1_0000;

/// Removes the scratch directory however the test ends.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A scratch directory of this case's own, inside this repository, where
/// the guest that runs `mkfs.btrfs` can see it.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(temp_path!("incompat-{tag}"));
    std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    dir
}

/// A fresh image with `bits` OR-ed into the superblock's
/// `incompat_flags`, the checksum stamped again over the change.
fn image_with_incompat(name: &str, bits: u64) -> (Scratch, std::path::PathBuf) {
    let dir = scratch_dir(name);
    let scratch = Scratch(dir.clone());
    let path = dir.join("img");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let made = oracle("mkfs.btrfs").arg("-f").arg(&path).output();
    assert!(
        made.status.success(),
        "mkfs.btrfs failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut sb = vec![0u8; 4096];
    file.read_exact_at(&mut sb, SUPERBLOCK).unwrap();
    let at = offsets::INCOMPAT_FLAGS;
    let flags = u64::from_le_bytes(sb[at..at + 8].try_into().unwrap());
    sb[at..at + 8].copy_from_slice(&(flags | bits).to_le_bytes());
    stamp_checksum(&mut sb, ChecksumType::Crc32c);
    file.write_all_at(&sb, SUPERBLOCK).unwrap();
    (scratch, path)
}

fn mount(path: &std::path::Path) -> fs_btrfs::error::Result<Filesystem> {
    Filesystem::mount(Arc::new(FileDevice::open(path).unwrap()) as Arc<dyn BlockRead>)
}

fn mount_rw(path: &std::path::Path) -> fs_btrfs::error::Result<Filesystem> {
    Filesystem::mount_rw(Arc::new(FileDevice::open_rw(path).unwrap()) as Arc<dyn BlockDevice>)
}

/// Both mounts refuse the image, each with a message holding `needle`.
fn refused_naming(name: &str, path: &std::path::Path, needle: &str) {
    for (how, result) in [("read-only", mount(path)), ("read-write", mount_rw(path))] {
        match result {
            Err(Error::UnsupportedFeature(m)) => {
                assert!(
                    m.contains(needle),
                    "{name} {how}: the refusal must name {needle}: {m}"
                )
            }
            Err(other) => panic!("{name} {how}: refused for the wrong reason: {other:?}"),
            Ok(_) => panic!("{name}: mounted {how}"),
        }
    }
}

#[test]
fn an_extent_tree_v2_volume_is_refused_by_name() {
    let (_scratch, path) = image_with_incompat("etv2", incompat::EXTENT_TREE_V2);
    refused_naming("extent-tree-v2", &path, "extent-tree-v2");
}

/// The remap tree moves where block addresses resolve (#448). Neither the
/// guest kernel nor its btrfs-progs can make a volume that uses it, so the
/// bit is set on one btrfs-progs made, as for extent-tree-v2 above.
#[test]
fn a_remap_tree_volume_is_refused_by_name() {
    let (_scratch, path) = image_with_incompat("remap", incompat::REMAP_TREE);
    refused_naming("remap-tree", &path, "remap-tree");
}

/// A bit no feature is known by is still refused, by its value: naming
/// the features this driver knows must not swallow the ones it does not.
#[test]
fn an_unknown_incompat_bit_is_refused_by_its_value() {
    let (_scratch, path) = image_with_incompat("unknown", 1 << 40);
    refused_naming("bit 40", &path, "0x0000010000000000");
}

/// The control: the same image with no bit added mounts both ways, so a
/// refusal above is the bit's doing and not the restamped superblock's.
#[test]
fn the_image_without_the_bit_mounts_both_ways() {
    let (_scratch, path) = image_with_incompat("plain", 0);
    mount(&path).unwrap_or_else(|e| panic!("read-only: {e:?}"));
    mount_rw(&path).unwrap_or_else(|e| panic!("read-write: {e:?}"));
}
