//! An empty data block group relocated into a newly allocated chunk,
//! judged by what is not this crate (#397, part of #304).
//!
//! A fresh `mkfs.btrfs` volume has one data group and nothing in it, so
//! relocating it is the two transactions every balance is built from and
//! nothing else: a new chunk of the same profile allocated (its chunk
//! item, a device extent per stripe, an empty block group item, a
//! free-space record covering it, and the device's `bytes_used` raised),
//! then the old group removed (all of the same, taken away again).
//!
//! # The oracles
//!
//! `btrfs check --readonly` judges the volume after the relocation,
//! `dump-tree -t chunk` must show a chunk at the new address and none at
//! the old, and the kernel mounts it, reports `Data, single` from
//! `btrfs filesystem df`, writes a file into the new chunk, and reads it
//! back on a second mount, after which `btrfs check` must be clean again.
//! A group this does not relocate is refused with the image unchanged.

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::Error;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A scratch volume, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "16384", "-d", "single"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        Scratch(dir)
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open read-write"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write")
}

/// The groups that hold data and nothing else.
fn data_groups(fs: &Filesystem) -> Vec<BlockGroup> {
    fs.block_groups()
        .unwrap()
        .into_iter()
        .filter(|g| g.holds_data() && !g.holds_metadata())
        .collect()
}

/// The logical address of every `CHUNK_ITEM` `dump-tree -t chunk` lists.
fn dumped_chunks(image: &Path) -> Vec<u64> {
    dump_tree(image, "chunk")
        .lines()
        .filter_map(|line| {
            let key = line.split("key (").nth(1)?;
            let words: Vec<&str> = key.split([' ', ')']).collect();
            (words.get(1) == Some(&"CHUNK_ITEM")).then_some(())?;
            words.get(2)?.parse().ok()
        })
        .collect()
}

#[test]
fn an_empty_data_group_is_relocated_into_a_new_chunk_the_kernel_writes_into() {
    let scratch = Scratch::new("relocate-empty-group");
    let image = scratch.image();

    let (old, new) = {
        let mut fs = mount_rw(&image);
        let groups = data_groups(&fs);
        assert_eq!(
            groups.len(),
            1,
            "a fresh volume has one data group: {groups:?}"
        );
        let old = groups[0].start;
        assert_eq!(groups[0].used, 0, "mkfs leaves its data group empty");
        let new = fs.relocate_block_group(old, None).expect("relocate");
        assert_ne!(new, old, "the group moved");
        let after = data_groups(&fs);
        assert_eq!(
            after
                .iter()
                .map(|g| (g.start, g.length, g.flags))
                .collect::<Vec<_>>(),
            vec![(new, groups[0].length, groups[0].flags)],
            "one data group, at the new address, of the old one's size and profile"
        );
        (old, new)
    };

    assert_btrfs_check_clean(&image, "after the relocation");
    let chunks = dumped_chunks(&image);
    assert!(
        chunks.contains(&new),
        "dump-tree has no chunk at {new}: {chunks:?}"
    );
    assert!(
        !chunks.contains(&old),
        "dump-tree still has the chunk at {old}: {chunks:?}"
    );

    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "write into the relocated group",
        "cd \"$MNT\"\n\
         btrfs filesystem df \"$MNT\"\n\
         head -c 1048576 /dev/urandom > after\n\
         sync\n\
         echo \"sum $(sha256sum after | cut -d' ' -f1)\"",
    );
    assert!(
        wrote.lines().any(|l| l.starts_with("Data, single")),
        "btrfs filesystem df does not show Data, single:\n{wrote}"
    );
    let written = wrote
        .lines()
        .find_map(|l| l.strip_prefix("sum "))
        .unwrap_or_else(|| panic!("no checksum in the kernel's output:\n{wrote}"))
        .to_string();
    let read = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "read back from the relocated group",
        "cd \"$MNT\"\necho \"sum $(sha256sum after | cut -d' ' -f1)\"",
    );
    assert!(
        read.lines()
            .any(|l| l.strip_prefix("sum ") == Some(written.as_str())),
        "the kernel read back other bytes than it wrote ({written}):\n{read}"
    );
    assert_btrfs_check_clean(&image, "after the kernel wrote into the new chunk");
}

#[test]
fn a_group_this_does_not_relocate_is_refused_before_anything_is_written() {
    let scratch = Scratch::new("relocate-refused");
    let image = scratch.image();
    let before = sha256_hex(&std::fs::read(&image).unwrap());

    let mut fs = mount_rw(&image);
    let metadata = fs
        .block_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.holds_metadata())
        .expect("a metadata group");
    let err = fs
        .relocate_block_group(metadata.start, None)
        .expect_err("a metadata group is refused");
    assert!(
        matches!(&err, Error::UnsupportedFeature(why) if why.contains("tree blocks")
            || why.contains("chunk tree")),
        "{err}"
    );
    let err = fs
        .relocate_block_group(1, None)
        .expect_err("no group starts at 1");
    assert!(matches!(err, Error::NotFound), "{err}");
    drop(fs);
    assert_eq!(
        sha256_hex(&std::fs::read(&image).unwrap()),
        before,
        "a refusal wrote to the image"
    );
}
