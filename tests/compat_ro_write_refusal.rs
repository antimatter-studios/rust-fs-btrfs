//! A volume with a `compat_ro` feature this driver does not maintain
//! mounts read-only and refuses to mount read-write (#72).
//!
//! `mount_rw` consulted `compat_ro_flags` for nothing, and this driver
//! writes: a block group tree volume would be written by code that looks
//! for block group items in the extent tree and finds none. The image is
//! made fresh by `mkfs.btrfs` on a plain file -- no mount, no root -- and
//! the test skips when btrfs-progs is not installed, unless
//! `BTRFS_ORACLE_FIXTURES=required`. The CI job that installs btrfs-progs
//! sets that, so there a missing `mkfs.btrfs` fails rather than reading as a
//! pass.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::stamp_checksum;
use fs_btrfs::superblock::{compat_ro, ChecksumType};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::process::Command;
use std::sync::Arc;

const SUPERBLOCK: usize = 0x1_0000;
const COMPAT_RO_FLAGS: usize = 0xb4;

/// A fresh image with `bits` OR-ed into the primary superblock's
/// `compat_ro_flags`, or `None` without `mkfs.btrfs`.
fn image_with_compat_ro(name: &str, bits: u64) -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("btrfs-compat-ro-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("img");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let made = match Command::new("mkfs.btrfs").arg("-f").arg(&path).output() {
        Ok(made) => made,
        Err(e) => {
            assert!(
                std::env::var("BTRFS_ORACLE_FIXTURES").as_deref() != Ok("required"),
                "BTRFS_ORACLE_FIXTURES=required, but mkfs.btrfs is not runnable: {e}"
            );
            return None;
        }
    };
    assert!(
        made.status.success(),
        "mkfs.btrfs failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    let mut bytes = std::fs::read(&path).unwrap();
    let sb = &mut bytes[SUPERBLOCK..SUPERBLOCK + 4096];
    let flags = u64::from_le_bytes(sb[COMPAT_RO_FLAGS..COMPAT_RO_FLAGS + 8].try_into().unwrap());
    sb[COMPAT_RO_FLAGS..COMPAT_RO_FLAGS + 8].copy_from_slice(&(flags | bits).to_le_bytes());
    stamp_checksum(sb, ChecksumType::Crc32c);
    std::fs::write(&path, &bytes).unwrap();
    Some(path)
}

#[test]
fn an_unmaintained_compat_ro_feature_mounts_read_only_and_refuses_read_write() {
    for (name, bits) in [
        ("bgt", compat_ro::BLOCK_GROUP_TREE),
        ("verity", compat_ro::VERITY),
        ("future", 1 << 40),
    ] {
        let Some(path) = image_with_compat_ro(name, bits) else {
            eprintln!("no mkfs.btrfs -- skipping");
            return;
        };
        let ro = FileDevice::open(&path).unwrap();
        Filesystem::mount(Arc::new(ro) as Arc<dyn BlockRead>)
            .unwrap_or_else(|e| panic!("{name}: a compat_ro bit must not stop a read: {e:?}"));

        let rw = FileDevice::open_rw(&path).unwrap();
        match Filesystem::mount_rw(Arc::new(rw) as Arc<dyn BlockDevice>) {
            Err(fs_btrfs::error::Error::UnsupportedFeature(m)) => {
                assert!(m.contains("can be read but not written"), "{name}: {m}")
            }
            Err(other) => panic!("{name}: refused for the wrong reason: {other:?}"),
            Ok(_) => panic!("{name}: mounted read-write over a feature it does not maintain"),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

/// The control: a fresh `mkfs.btrfs` image, whose free-space tree bits
/// this driver maintains, still mounts read-write.
#[test]
fn a_fresh_image_still_mounts_read_write() {
    let Some(path) = image_with_compat_ro("fresh", 0) else {
        eprintln!("no mkfs.btrfs -- skipping");
        return;
    };
    let rw = FileDevice::open_rw(&path).unwrap();
    Filesystem::mount_rw(Arc::new(rw) as Arc<dyn BlockDevice>)
        .unwrap_or_else(|e| panic!("a fresh image must mount read-write: {e:?}"));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
