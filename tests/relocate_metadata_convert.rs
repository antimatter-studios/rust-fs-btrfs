//! A metadata block group converted between DUP and single by relocation
//! (#588), on this crate's own volume, whose metadata mkfs makes DUP: no
//! tool, no fixture, no VM. What btrfs-progs and the kernel make of a
//! kernel-written volume converted the same way is in
//! tests/relocate_metadata_group_kernel.rs.

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::chunk::block_group::SYSTEM;
use fs_btrfs::{mkfs, ChunkProfile, Error, Filesystem};
use fs_btrfs_test_support::temp_path;
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A fresh volume made by this crate's mkfs, removed when it drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let image = PathBuf::from(temp_path!("relocate-metadata-convert-{tag}.img"));
        std::fs::create_dir_all(image.parent().unwrap()).unwrap();
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let dev = FileDevice::open_rw(&image).unwrap();
        mkfs::format(&dev, &mkfs::Options::default()).unwrap();
        drop(dev);
        Self(image)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open read-write"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write")
}

fn metadata_group(fs: &Filesystem) -> BlockGroup {
    fs.block_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.holds_metadata() && g.flags & SYSTEM == 0)
        .expect("a metadata group")
}

fn payload(i: u32) -> Vec<u8> {
    (0..3000u32).map(|b| (b * 13 + i * 7) as u8).collect()
}

/// Sixty-four files written through the library, so the metadata group
/// holds leaves to move; returns each one's inode and bytes.
fn populate(image: &Path) -> Vec<(u64, Vec<u8>)> {
    let mut fs = mount_rw(image);
    let root = fs.lookup_path("/").unwrap().ino;
    (0..64)
        .map(|i| {
            let ino = fs
                .create(root, format!("f{i}").as_bytes(), 0o644, 0, 0)
                .unwrap();
            let bytes = payload(i);
            fs.write(ino, 0, &bytes).unwrap();
            (ino, bytes)
        })
        .collect()
}

#[test]
fn a_dup_metadata_group_converts_to_single_and_back() {
    let scratch = Scratch::new("round-trip");
    let files = populate(&scratch.0);
    let mut start = {
        let fs = mount_rw(&scratch.0);
        let group = metadata_group(&fs);
        assert_eq!(
            ChunkProfile::from_type(group.flags).unwrap(),
            ChunkProfile::Dup,
            "this crate's mkfs makes DUP metadata"
        );
        group.start
    };
    for to in [ChunkProfile::Single, ChunkProfile::Dup] {
        let mut fs = mount_rw(&scratch.0);
        let old = fs
            .block_groups()
            .unwrap()
            .into_iter()
            .find(|g| g.start == start)
            .unwrap();
        let new = fs
            .relocate_block_group(start, Some(to))
            .unwrap_or_else(|e| panic!("convert the metadata group to {}: {e}", to.name()));
        let groups = fs.block_groups().unwrap();
        assert!(
            groups.iter().all(|g| g.start != start),
            "the old group is gone"
        );
        let replacement = groups.iter().find(|g| g.start == new).unwrap();
        assert!(replacement.holds_metadata() && replacement.flags & SYSTEM == 0);
        assert_eq!(ChunkProfile::from_type(replacement.flags).unwrap(), to);
        assert_eq!(replacement.length, old.length);
        assert!(
            replacement.used > 0,
            "tree blocks moved into the {} group",
            to.name()
        );
        drop(fs);
        let fs = mount_rw(&scratch.0);
        for (ino, bytes) in &files {
            assert_eq!(
                &fs.read_file(*ino).unwrap(),
                bytes,
                "inode {ino} after converting metadata to {}",
                to.name()
            );
        }
        start = new;
    }
}

#[test]
fn other_metadata_conversions_and_the_system_group_are_refused_before_writing() {
    let scratch = Scratch::new("refused");
    let before = std::fs::read(&scratch.0).unwrap();
    let mut fs = mount_rw(&scratch.0);
    let metadata = metadata_group(&fs);
    let system = fs
        .block_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.flags & SYSTEM != 0)
        .expect("a system group");
    for (start, profile, reason) in [
        (metadata.start, Some(ChunkProfile::Raid1), "converting"),
        (metadata.start, None, "only single"),
        (system.start, Some(ChunkProfile::Single), "system"),
    ] {
        let error = fs.relocate_block_group(start, profile).unwrap_err();
        assert!(
            matches!(&error, Error::UnsupportedFeature(why) if why.contains(reason)),
            "expected {reason}: {error}"
        );
    }
    drop(fs);
    assert!(
        std::fs::read(&scratch.0).unwrap() == before,
        "a refusal wrote to the image"
    );
}
