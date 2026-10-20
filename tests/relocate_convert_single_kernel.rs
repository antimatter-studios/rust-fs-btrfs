//! A DUP data block group of the kernel's files, relocated into a new
//! single chunk, judged by what is not this crate (#587, part of #400).
//!
//! The kernel makes the data on a fresh DUP-data volume and a SHA-256
//! manifest is taken from its own reading before this crate touches the
//! volume. One copy of a sector of two different extents is corrupted
//! first, the first copy of one and the second copy of the other, so the
//! conversion has to read each extent from whichever copy verifies.
//!
//! # The oracles
//!
//! `btrfs inspect-internal dump-tree` names the old chunk's two stripes
//! and the new chunk's one, and shows the old chunk and both its device
//! extents gone. `btrfs check --readonly --check-data-csum` judges
//! metadata and data digests. `btrfs filesystem df` reports
//! `Data, single` and no DUP data, and the kernel reads every file of the
//! manifest back with the same digests.

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::chunk::{block_group as bg, ChunkProfile};
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
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
            .args(["-q", "-f", "-d", "dup", "-s", "4096", "-n", "16384"])
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

/// `name digest` for every file the scripts write, as the kernel reads it.
const MANIFEST: &str = "cd \"$MNT\"\n\
     for f in a b c d; do echo \"sum $f $(sha256sum \"$f\" | cut -d' ' -f1)\"; done";

fn manifest(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix("sum "))
        .filter_map(|l| l.split_once(' '))
        .map(|(name, sum)| (name.to_string(), sum.to_string()))
        .collect()
}

/// The physical offsets of the stripes of the chunk at `logical`, as
/// `btrfs inspect-internal dump-tree -t chunk` prints them, after
/// checking the chunk's type is one of `types`.
fn stripe_offsets(image: &Path, logical: u64, types: &[&str]) -> Vec<u64> {
    let chunks = dump_tree(image, "chunk");
    let head = format!("CHUNK_ITEM {logical})");
    let item = chunks
        .split("\titem ")
        .find(|item| item.contains(&head))
        .unwrap_or_else(|| panic!("no chunk item at {logical}:\n{chunks}"));
    assert!(
        item.lines()
            .filter_map(|l| l.split_whitespace().skip_while(|w| *w != "type").nth(1))
            .any(|ty| types.contains(&ty)),
        "the chunk at {logical} is not {types:?}: {item}"
    );
    item.lines()
        .filter_map(|l| l.trim().strip_prefix("stripe "))
        .map(|l| {
            l.split_whitespace()
                .skip_while(|w| *w != "offset")
                .nth(1)
                .and_then(|w| w.parse().ok())
                .unwrap_or_else(|| panic!("a stripe line with no offset: {l}"))
        })
        .collect()
}

/// Overwrite the sector at physical `at` with a pattern no digest matches.
fn corrupt(image: &Path, at: u64) {
    let mut file = std::fs::OpenOptions::new().write(true).open(image).unwrap();
    file.seek(SeekFrom::Start(at)).unwrap();
    file.write_all(&[0xa5; 4096]).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn a_dup_data_group_is_converted_to_single_reading_each_extent_from_the_copy_that_verifies() {
    let scratch = Scratch::new("relocate-convert-single");
    let image = scratch.image();

    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "write the files to convert",
        &format!(
            "cd \"$MNT\"\n\
             head -c 65536 /dev/urandom > a\n\
             head -c 200704 /dev/urandom > b\n\
             head -c 12288 /dev/urandom > c\n\
             head -c 1048576 /dev/urandom > d\n\
             sync\n\
             head -c 40960 /dev/urandom >> a\n\
             head -c 131072 /dev/urandom >> d\n\
             sync\n\
             {MANIFEST}"
        ),
    );
    let before = manifest(&wrote);
    assert_eq!(before.len(), 4, "the kernel reported:\n{wrote}");

    let (old, extents) = {
        let fs = mount_rw(&image);
        let groups = data_groups(&fs);
        assert_eq!(groups.len(), 1, "one data group: {groups:?}");
        let old = groups[0];
        assert_eq!(
            ChunkProfile::from_type(old.flags).unwrap(),
            ChunkProfile::Dup
        );
        assert!(old.used > 0, "the kernel's files are in the data group");
        let extents: Vec<(u64, u64)> = fs
            .block_group_extents(old.start)
            .unwrap()
            .iter()
            .map(|e| (e.start, e.len))
            .collect();
        assert!(extents.len() >= 2, "two extents to corrupt: {extents:?}");
        (old, extents)
    };

    // One copy of the largest extent's first sector and the other copy of
    // the smallest's: whichever copy is read first, one extent needs the
    // other.
    let stripes = stripe_offsets(&image, old.start, &["DATA|DUP"]);
    assert_eq!(stripes.len(), 2, "a DUP chunk has two stripes: {stripes:?}");
    let &(largest, _) = extents.iter().max_by_key(|e| e.1).unwrap();
    let &(smallest, _) = extents.iter().min_by_key(|e| e.1).unwrap();
    assert_ne!(largest, smallest);
    corrupt(&image, stripes[0] + (largest - old.start));
    corrupt(&image, stripes[1] + (smallest - old.start));

    let new = {
        let mut fs = mount_rw(&image);
        let new = fs
            .relocate_block_group(old.start, Some(ChunkProfile::Single))
            .expect("convert the populated DUP data group to single");
        let after = data_groups(&fs);
        assert_eq!(
            after
                .iter()
                .map(|g| (g.start, g.length, g.flags, g.used))
                .collect::<Vec<_>>(),
            vec![(new, old.length, bg::DATA, old.used)],
            "one single data group, at the new address, of the old one's size and use"
        );
        new
    };

    assert_btrfs_check_clean(&image, "after the conversion");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let one = stripe_offsets(&image, new, &["DATA", "DATA|single"]);
    assert_eq!(one.len(), 1, "a single chunk has one stripe: {one:?}");
    let chunks = dump_tree(&image, "chunk");
    assert!(
        !chunks.contains(&format!("CHUNK_ITEM {})", old.start)),
        "the DUP chunk is still in the chunk tree:\n{chunks}"
    );
    let devices = dump_tree(&image, "dev");
    for at in &stripes {
        assert!(
            !devices.contains(&format!("DEV_EXTENT {at})")),
            "the DUP chunk's device extent at {at} is still in the device tree:\n{devices}"
        );
    }

    let read = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "read the converted files and report the profiles",
        &format!("{MANIFEST}\nbtrfs filesystem df \"$MNT\""),
    );
    assert!(
        read.lines().any(|line| line.starts_with("Data, single")),
        "{read}"
    );
    assert!(
        !read.lines().any(|line| line.starts_with("Data, DUP")),
        "{read}"
    );
    assert_eq!(
        manifest(&read),
        before,
        "the kernel reads other bytes after the conversion:\n{read}"
    );
    assert_btrfs_check_clean(&image, "after the kernel reads the single group");
}
