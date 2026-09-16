//! The superblock is read from every copy, and a mount uses the newest
//! valid one (#90).
//!
//! Every read hardcoded copy 0, so a volume whose primary superblock was
//! damaged could not be mounted though the commit path writes all three.
//! A fresh `mkfs.btrfs` image of 256 MiB has copies 0 and 1 and no copy 2,
//! so the absent copy is exercised by every case. Skips without
//! btrfs-progs.

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::stamp_checksum;
use fs_btrfs::superblock::{offsets, read_superblock, ChecksumType, SUPER_OFFSETS};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::process::Command;
use std::sync::Arc;

fn fresh_image(name: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("btrfs-sb-mirrors-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("img");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let made = Command::new("mkfs.btrfs")
        .arg("-f")
        .arg(&path)
        .output()
        .ok()?;
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    Some(path)
}

fn edit_copy(path: &std::path::Path, copy: usize, edit: impl FnOnce(&mut [u8])) {
    let mut bytes = std::fs::read(path).unwrap();
    let at = SUPER_OFFSETS[copy] as usize;
    edit(&mut bytes[at..at + 4096]);
    std::fs::write(path, &bytes).unwrap();
}

fn read(path: &std::path::Path) -> (u64, usize) {
    let dev = FileDevice::open(path).unwrap();
    let (sb, copy) = read_superblock(&dev).expect("a valid copy");
    (sb.generation, copy)
}

fn mount_ro(path: &std::path::Path) -> Result<Filesystem, Error> {
    Filesystem::mount(Arc::new(FileDevice::open(path).unwrap()) as Arc<dyn BlockRead>)
}

fn mount_rw(path: &std::path::Path) -> Result<Filesystem, Error> {
    Filesystem::mount_rw(Arc::new(FileDevice::open_rw(path).unwrap()) as Arc<dyn BlockDevice>)
}

/// Control: an undamaged volume reads copy 0, and the absent copy 2 is
/// not an error for either mount.
#[test]
fn an_undamaged_volume_uses_the_primary_and_its_absent_third_copy_is_not_damage() {
    let Some(path) = fresh_image("control") else {
        eprintln!("no mkfs.btrfs -- skipping");
        return;
    };
    assert_eq!(read(&path).1, 0);
    mount_ro(&path).expect("mounts read-only");
    mount_rw(&path).expect("mounts read-write");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A zeroed primary mounts from copy 1, read-only, and refuses to be
/// written until it has been checked.
#[test]
fn a_zeroed_primary_mounts_read_only_from_the_mirror() {
    let Some(path) = fresh_image("zeroed") else {
        eprintln!("no mkfs.btrfs -- skipping");
        return;
    };
    edit_copy(&path, 0, |sb| sb.fill(0));
    assert_eq!(read(&path).1, 1, "the mirror was not used");
    mount_ro(&path).unwrap_or_else(|e| panic!("a zeroed primary must mount from copy 1: {e:?}"));
    match mount_rw(&path) {
        Err(Error::UnsupportedFeature(m)) => assert!(m.contains("copy 1"), "{m}"),
        other => panic!(
            "a mirror mount was allowed to write: {:?}",
            other.map(|_| ())
        ),
    }
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Two valid copies that disagree: the newer generation wins, which is
/// what a commit torn between its copies needs.
#[test]
fn the_copy_with_the_newer_generation_wins() {
    let Some(path) = fresh_image("newer") else {
        eprintln!("no mkfs.btrfs -- skipping");
        return;
    };
    let (gen0, _) = read(&path);
    edit_copy(&path, 1, |sb| {
        sb[offsets::GENERATION..offsets::GENERATION + 8].copy_from_slice(&(gen0 + 1).to_le_bytes());
        stamp_checksum(sb, ChecksumType::Crc32c);
    });
    assert_eq!(
        read(&path),
        (gen0 + 1, 1),
        "the older primary was preferred"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
