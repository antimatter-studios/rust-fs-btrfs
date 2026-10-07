//! RAID5 and RAID6 pools read what the kernel wrote, whole and with
//! devices damaged (#268).
//!
//! The fixtures are made by the kernel in the harness VM (`chore
//! fixtures`): RAID5 over three devices and RAID6 over four, data AND
//! metadata, so the chunk tree, every other tree and every file extent
//! live in parity chunks. Their manifests give each file's size and
//! SHA-256 as the kernel read it while mounted, and `btrfs check` found
//! each pool clean before it was published.
//!
//! # What each test is a check on
//!
//! - **Whole pools.** Parity rotates one device per full stripe. A reader
//!   that placed data elements by any other rule reads a parity element as
//!   data somewhere in the 8 MiB file, and the digest catches it.
//! - **One device damaged.** Every read the damaged device answers is
//!   garbage, not an error, which is the case only a checksum can catch.
//!   Every tree block and data sector on it has to be rebuilt from the
//!   others and P, and the digest says whether the rebuilt bytes are the
//!   kernel's.
//! - **Two RAID6 devices damaged, every pair.** A full stripe then loses
//!   a data element and P (rebuilt from Q), a data element and Q (from P),
//!   or two data elements (from P and Q together), so all three
//!   reconstructions are used, with nothing but the kernel's bytes to
//!   agree with.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{fixture, sha256_hex};
use fs_core::{BlockRead, FileDevice};
use std::sync::Arc;

/// Where damage begins on a device: past the primary superblock at
/// 64 KiB, so the device still says which filesystem and which device it
/// is, as a disk with bad sectors does.
const DAMAGE_FROM: u64 = 1024 * 1024;

/// The superblock copy at 64 MiB, kept whole for the same reason.
const SUPER_COPY: std::ops::Range<u64> = 64 * 1024 * 1024..64 * 1024 * 1024 + 4096;

/// A device that returns garbage, with success, for every byte from
/// [`DAMAGE_FROM`] on. Silent corruption, not a read error: only a
/// checksum can tell.
struct Damaged(FileDevice);

impl BlockRead for Damaged {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.0.read_at(offset, buf)?;
        for (i, b) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            if at >= DAMAGE_FROM && !SUPER_COPY.contains(&at) {
                *b = 0xA5 ^ (at as u8);
            }
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.size_bytes()
    }
}

fn members(profile: &str, count: usize) -> Vec<std::path::PathBuf> {
    (1..=count)
        .map(|i| fixture(&format!("btrfs-{profile}-{i}.img")))
        .collect()
}

/// `(path, size, sha256)` for every file in the manifest.
fn manifest(profile: &str) -> Vec<(String, usize, String)> {
    let path = fixture(&format!("btrfs-{profile}.manifest"));
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
        files.len() >= 5,
        "the {profile} manifest names {} files; the fixture builder writes five",
        files.len()
    );
    files
}

/// Open the pool with the devices whose index is in `damaged` damaged,
/// and require every file to read back as the kernel wrote it.
fn reads_what_the_kernel_wrote(profile: &str, count: usize, damaged: &[usize]) {
    let devices: Vec<Arc<dyn BlockRead>> = members(profile, count)
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let dev = FileDevice::open(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            if damaged.contains(&i) {
                Arc::new(Damaged(dev)) as Arc<dyn BlockRead>
            } else {
                Arc::new(dev) as Arc<dyn BlockRead>
            }
        })
        .collect();
    let what = format!("{profile} with devices {damaged:?} damaged");
    let fs = Filesystem::mount_pool(devices).unwrap_or_else(|e| panic!("{what}: mount: {e}"));
    for (path, size, digest) in manifest(profile) {
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

#[test]
fn a_raid5_pool_reads_what_the_kernel_wrote() {
    reads_what_the_kernel_wrote("raid5", 3, &[]);
}

#[test]
fn a_raid6_pool_reads_what_the_kernel_wrote() {
    reads_what_the_kernel_wrote("raid6", 4, &[]);
}

#[test]
fn a_raid5_pool_survives_any_one_damaged_device() {
    for d in 0..3 {
        reads_what_the_kernel_wrote("raid5", 3, &[d]);
    }
}

#[test]
fn a_raid6_pool_survives_any_one_damaged_device() {
    for d in 0..4 {
        reads_what_the_kernel_wrote("raid6", 4, &[d]);
    }
}

#[test]
fn a_raid6_pool_survives_any_two_damaged_devices() {
    for a in 0..4 {
        for b in a + 1..4 {
            reads_what_the_kernel_wrote("raid6", 4, &[a, b]);
        }
    }
}

/// Two damaged devices are one more than RAID5 can rebuild from, and the
/// answer is an error, never bytes that are not the kernel's.
#[test]
fn a_raid5_pool_with_two_damaged_devices_fails_rather_than_guessing() {
    let devices: Vec<Arc<dyn BlockRead>> = members("raid5", 3)
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let dev = FileDevice::open(p).expect("open");
            if i < 2 {
                Arc::new(Damaged(dev)) as Arc<dyn BlockRead>
            } else {
                Arc::new(dev) as Arc<dyn BlockRead>
            }
        })
        .collect();
    let Ok(fs) = Filesystem::mount_pool(devices) else {
        return; // Refused at mount: no bytes handed out at all.
    };
    for (path, _, digest) in manifest("raid5") {
        if let Ok(got) = fs.read_path(&path) {
            assert_eq!(
                sha256_hex(&got),
                digest,
                "{path}: bytes handed back that are not the kernel's"
            );
        }
    }
}
