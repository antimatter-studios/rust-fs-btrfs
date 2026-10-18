//! Emulated zoned volumes retain fixed superblocks and ordinary read
//! mappings (#454). The guest's static mkfs formats the fixture, the
//! kernel writes its files and manifest, and btrfs check validates it
//! before publication. No real zoned device's superblock log is read.

use fs_btrfs::{error::Error, fs::Filesystem, superblock::incompat};
use fs_btrfs_test_support::{fixture, sha256_hex};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::sync::Arc;

fn device() -> Arc<WriteForbidden> {
    Arc::new(WriteForbidden(
        FileDevice::open(fixture("features/btrfs-zoned.img")).expect("open zoned fixture"),
    ))
}

/// Advertise write capability but fail on any attempted write, so a
/// refusal proves it happened without changing the fixture.
struct WriteForbidden(FileDevice);

impl BlockRead for WriteForbidden {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.0.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.0.size_bytes()
    }
}

impl BlockDevice for WriteForbidden {
    fn write_at(&self, _: u64, _: &[u8]) -> fs_core::Result<()> {
        panic!("a zoned volume must be refused before any write")
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn mount() -> Filesystem {
    let fs = Filesystem::mount(device()).expect("an emulated zoned volume mounts read-only");
    assert_ne!(fs.superblock().incompat_flags & incompat::ZONED, 0);
    fs
}

#[test]
fn every_file_reads_what_the_kernel_wrote() {
    let fs = mount();
    let manifest =
        std::fs::read_to_string(fixture("features/btrfs-zoned.manifest")).expect("kernel manifest");
    let mut checked = 0;
    for line in manifest.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 3, "{line:?}");
        let got = fs.read_path(fields[0]).expect("read kernel-written file");
        assert_eq!(got.len().to_string(), fields[1], "{}: size", fields[0]);
        assert_eq!(sha256_hex(&got), fields[2], "{}: contents", fields[0]);
        checked += 1;
    }
    assert!(checked >= 5, "the manifest contains only {checked} files");
}

#[test]
fn fsck_finds_the_volume_clean_as_btrfs_check_does() {
    let report = fs_btrfs::check::check(&mount());
    assert!(report.findings.is_empty(), "{:#?}", report.findings);
}

fn refused(result: Result<Filesystem, Error>) {
    match result {
        Err(Error::UnsupportedFeature(message)) => {
            assert!(message.contains("zoned"), "{message}");
            assert!(message.contains("read"), "{message}");
        }
        other => panic!(
            "a zoned write mount was not refused by name: {:?}",
            other.map(|_| ())
        ),
    }
}

#[test]
fn a_read_write_mount_refuses_zoned_mode_without_writing() {
    refused(Filesystem::mount_rw(device()));
}

#[test]
fn a_pool_write_mount_refuses_zoned_mode_without_writing() {
    refused(Filesystem::mount_pool_rw(vec![device()]));
}
