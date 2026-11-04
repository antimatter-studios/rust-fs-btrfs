//! `fsck.btrfs` agrees with `btrfs check --readonly` about what is wrong
//! (#260).
//!
//! Two halves, each against the reference checker in the harness guest:
//!
//! - **Clean volumes stay clean.** Every fixture below was made by the
//!   standard formatter and filled by the kernel. `btrfs check` must call
//!   each clean, and so must `fsck.btrfs` (exit 0).
//! - **Damage is found.** A copy of a populated fixture is damaged one way
//!   at a time: an item's bytes are changed in place and the block's
//!   checksum recomputed and written to every mirror, so each case is the
//!   damage it names and not a checksum failure. `btrfs check` must find
//!   each one (a nonzero status), or the case is not damage and the test
//!   says so; then `fsck.btrfs` must find it too (exit 4).

mod cli_support;

use cli_support::*;
use fs_btrfs::chunk::DiskKey;
use fs_btrfs::Filesystem;
use fs_btrfs_test_support::{copy_sparse, fixture, oracle, temp_dir};
use fs_core::{BlockRead, FileDevice};
use std::ffi::OsStr;
use std::ops::Deref;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Fixtures the reference checker calls clean.
const CLEAN: &[&str] = &[
    "btrfs-default.img",
    "btrfs-dup.img",
    "btrfs-single.img",
    "btrfs-mixed.img",
    "btrfs-rich.img",
    "btrfs-subvol.img",
    "btrfs-xattr.img",
    "btrfs-nodatacow.img",
    "btrfs-comp-zstd.img",
    "btrfs-csum-sha256.img",
    "btrfs-node4k.img",
    "btrfs-deep16k.img",
];

/// The populated fixture each damage is made in.
const DAMAGED_FROM: &str = "btrfs-rich.img";

/// What this binary's copies may hold on the disk at once, allocated.
///
/// One copy of the largest fixture checked here: btrfs-deep16k.img holds
/// 113.9 MiB of its 2048 MiB, so 150 MiB is about 30% over it. A copy of every
/// fixture is 11984 MiB when each one is written out in full, and on a runner
/// with 14 GB free that ran out of disk part way through (#731).
const SCRATCH_BUDGET_MIB: u64 = 150;

fn fsck(image: &Path) -> (Option<i32>, String) {
    let out = tool("fsck.btrfs")
        .arg("--text")
        .arg(image)
        .output()
        .expect("spawn fsck.btrfs");
    (
        out.status.code(),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

/// `btrfs check --readonly`: clean, and what it said.
fn reference(image: &Path) -> (bool, String) {
    let out = oracle("btrfs")
        .args(["check", "--readonly"])
        .arg(image)
        .output();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// A fixture's copy, and this binary's one turn to hold one. Dropping it removes
/// the copy and passes the turn on, so the tests running beside each other hold
/// one copy between them and not one per case.
struct ScratchCopy {
    image: PathBuf,
    _turn: MutexGuard<'static, ()>,
}

impl Deref for ScratchCopy {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.image
    }
}

impl AsRef<OsStr> for ScratchCopy {
    fn as_ref(&self) -> &OsStr {
        self.image.as_os_str()
    }
}

impl Drop for ScratchCopy {
    fn drop(&mut self) {
        // A copy left behind fails the next test to take a copy, on the budget.
        if let Some(dir) = self.image.parent() {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                eprintln!("remove {}: {e}", dir.display());
            }
        }
    }
}

fn copy(name: &str, tag: &str) -> ScratchCopy {
    static TURN: Mutex<()> = Mutex::new(());
    // A test that failed holding the turn has already had its copy removed.
    let turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let image = scratch_dir(&format!("fsck-{tag}")).join(name);
    let copy = ScratchCopy { image, _turn: turn };
    copy_sparse(fixture(name), &copy.image).unwrap_or_else(|e| panic!("copy {name}: {e}"));
    assert_scratch_within_budget(name);
    copy
}

/// The bytes `path` holds on the disk, and all it contains if it is a directory.
/// A path another test has just removed holds none.
fn allocated(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let mut total = meta.blocks() * 512;
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            total += entries
                .flatten()
                .map(|entry| allocated(&entry.path()))
                .sum::<u64>();
        }
    }
    total
}

/// Fails the test when this binary's scratch holds more than its budget.
#[track_caller]
fn assert_scratch_within_budget(just_copied: &str) {
    let mine = format!("cli-{}-fsck-", std::process::id());
    let held: u64 = std::fs::read_dir(temp_dir())
        .expect("read the scratch directory")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&mine))
        .map(|entry| allocated(&entry.path()))
        .sum();
    assert!(
        held <= SCRATCH_BUDGET_MIB << 20,
        "with {just_copied} copied, this binary's scratch holds {} MiB of the disk, over its \
         budget of {SCRATCH_BUDGET_MIB} MiB: it holds more than one copy, or a copy is written \
         out in full",
        held >> 20
    );
}

#[test]
fn every_volume_the_reference_calls_clean_is_clean() {
    for name in CLEAN {
        let image = copy(name, &format!("clean-{name}"));
        let (clean, said) = reference(&image);
        assert!(
            clean,
            "{name}: btrfs check does not call the fixture clean:\n{said}"
        );
        let (code, ours) = fsck(&image);
        assert_eq!(
            code,
            Some(0),
            "{name}: fsck.btrfs finds fault with a clean volume:\n{ours}"
        );
    }
}

fn add_u64(data: &mut [u8], at: usize, delta: u64) {
    let v = u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    data[at..at + 8].copy_from_slice(&v.wrapping_add(delta).to_le_bytes());
}
fn add_u32(data: &mut [u8], at: usize, delta: u32) {
    let v = u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    data[at..at + 4].copy_from_slice(&v.wrapping_add(delta).to_le_bytes());
}

/// A regular file's inode in the default subvolume.
fn is_file_inode(k: &DiskKey, data: &[u8]) -> bool {
    k.key_type == 1
        && k.objectid > 256
        && u32::from_le_bytes(data[52..56].try_into().unwrap()) & 0o170000 == 0o100000
}

/// One way of damaging an image in place.
type Damage = Box<dyn Fn(&Path)>;

#[test]
fn every_damage_the_reference_finds_is_found() {
    let cases: Vec<(&str, Damage)> = vec![
        (
            "link-count",
            Box::new(|p| edit_item(p, 5, is_file_inode, |d| add_u32(d, 40, 1))),
        ),
        (
            "directory-size",
            Box::new(|p| {
                edit_item(
                    p,
                    5,
                    |k, _| k.key_type == 1 && k.objectid == 256,
                    |d| add_u64(d, 16, 2),
                )
            }),
        ),
        (
            "block-group-used",
            Box::new(|p| edit_item(p, 2, |k, _| k.key_type == 192, |d| add_u64(d, 0, 4096))),
        ),
        (
            "device-bytes-used",
            Box::new(|p| edit_item(p, 3, |k, _| k.key_type == 216, |d| add_u64(d, 16, 1 << 20))),
        ),
        (
            "device-extent-length",
            Box::new(|p| edit_item(p, 4, |k, _| k.key_type == 204, |d| add_u64(d, 24, 1 << 20))),
        ),
        (
            "entry-to-no-inode",
            Box::new(|p| {
                edit_item(
                    p,
                    5,
                    |k, _| k.key_type == 96 && k.objectid == 256,
                    |d| add_u64(d, 0, 1000),
                )
            }),
        ),
        (
            "file-extent-length",
            Box::new(|p| {
                edit_item(
                    p,
                    5,
                    |k, d| k.key_type == 108 && d.len() >= 53 && d[20] == 1 && d[21..29] != [0; 8],
                    |d| add_u64(d, 29, 4096),
                )
            }),
        ),
        (
            "superblock-bytes-used",
            Box::new(|p| {
                let fs =
                    Filesystem::mount(Arc::new(FileDevice::open(p.to_str().unwrap()).unwrap()))
                        .unwrap();
                let csum = fs.superblock().csum_type;
                let size = FileDevice::open(p.to_str().unwrap()).unwrap().size_bytes();
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(p)
                    .unwrap();
                for at in [64u64 << 10, 64 << 20, 256 << 30] {
                    if at + 4096 > size {
                        continue;
                    }
                    let mut raw = vec![0u8; 4096];
                    file.read_exact_at(&mut raw, at).unwrap();
                    add_u64(&mut raw, 0x78, 4096);
                    fs_btrfs::super_write::stamp_checksum(&mut raw, csum);
                    file.write_all_at(&raw, at).unwrap();
                }
            }),
        ),
    ];

    for (name, damage) in cases {
        let image = copy(DAMAGED_FROM, name);
        damage(&image);
        let (clean, said) = reference(&image);
        assert!(
            !clean,
            "{name}: btrfs check finds nothing wrong, so this is not damage:\n{said}"
        );
        let (code, ours) = fsck(&image);
        assert_eq!(
            code,
            Some(4),
            "{name}: btrfs check finds the damage and fsck.btrfs does not:\n{ours}\n\
             --- btrfs check said:\n{said}"
        );
    }
}

#[test]
fn a_sparse_copy_reads_the_same_bytes_and_keeps_its_holes() {
    let dir = scratch_dir("fsck-sparse");
    let from = dir.join("from");
    let to = dir.join("to");
    let file = std::fs::File::create(&from).unwrap();
    // 64 MiB, ending in a hole: data at the start, and across the 1 MiB and
    // 4 KiB edges a copy works in.
    file.set_len(64 << 20).unwrap();
    file.write_all_at(b"start", 0).unwrap();
    file.write_all_at(&[0xa5; 8192], (1 << 20) - 4096).unwrap();
    file.write_all_at(&[0x5a], (40 << 20) + 4095).unwrap();
    drop(file);

    copy_sparse(&from, &to).expect("copy");

    assert!(
        std::fs::read(&from).unwrap() == std::fs::read(&to).unwrap(),
        "the copy reads differently from its source"
    );
    // Not exactly 16 KiB: APFS fills a gap of under about 1 MiB (1081344 bytes
    // measured). A copy written out in full takes all 64 MiB.
    let held = allocated(&to);
    assert!(
        held < 8 << 20,
        "a copy with 16 KiB of data in 64 MiB takes {held} bytes of the disk"
    );
}

#[test]
fn a_device_that_is_not_btrfs_is_an_operational_error() {
    let image = scratch_dir("fsck-zero").join("zero.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();
    let (code, said) = fsck(&image);
    assert_eq!(
        code,
        Some(8),
        "a zeroed device is not Btrfs, and nothing was checked:\n{said}"
    );
}

#[test]
fn a_repair_is_refused_not_pretended() {
    let image = copy("btrfs-default.img", "y");
    let out = tool("fsck.btrfs").arg("-y").arg(&image).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(16),
        "fsck.btrfs -y: {}",
        stderr(&out)
    );
}
