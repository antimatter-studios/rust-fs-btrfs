//! A single data block group of the kernel's files, relocated into a new
//! DUP chunk, judged by what is not this crate (#586, part of #400).
//!
//! The kernel makes the data on a fresh single-data volume and a SHA-256
//! manifest is taken from its own reading before this crate touches the
//! volume. The group is then relocated with a conversion to DUP, so
//! every moved sector has to be written to both stripes of the new chunk.
//!
//! # The oracles
//!
//! `btrfs inspect-internal dump-tree` names the new chunk's two stripes,
//! and their bytes are compared raw for every moved extent.
//! `btrfs check --readonly --check-data-csum` judges metadata and data
//! digests. One copy of a sector is then corrupted, and a kernel scrub
//! has to repair it from the other: afterwards both copies agree again,
//! `btrfs filesystem df` reports `Data, DUP` and no single data, and the
//! kernel reads every file of the manifest back with the same digests.

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::chunk::{block_group as bg, ChunkProfile};
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
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
            .args(["-q", "-f", "-d", "single", "-s", "4096", "-n", "16384"])
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
/// `btrfs inspect-internal dump-tree -t chunk` prints them.
fn stripe_offsets(image: &Path, logical: u64) -> Vec<u64> {
    let chunks = dump_tree(image, "chunk");
    let head = format!("CHUNK_ITEM {logical})");
    let item = chunks
        .split("\titem ")
        .find(|item| item.contains(&head))
        .unwrap_or_else(|| panic!("no chunk item at {logical}:\n{chunks}"));
    assert!(item.contains("type DATA|DUP"), "{item}");
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

fn read_raw(image: &Path, at: u64, len: u64) -> Vec<u8> {
    let mut file = std::fs::File::open(image).unwrap();
    file.seek(SeekFrom::Start(at)).unwrap();
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf).unwrap();
    buf
}

/// Every moved extent's bytes are the same in both stripes.
fn assert_copies_agree(
    image: &Path,
    chunk: u64,
    stripes: &[u64],
    extents: &[(u64, u64)],
    when: &str,
) {
    for &(at, len) in extents {
        let first = read_raw(image, stripes[0] + (at - chunk), len);
        let second = read_raw(image, stripes[1] + (at - chunk), len);
        assert!(
            first == second,
            "{when}: the two copies of the extent at {at} differ"
        );
    }
}

#[test]
fn a_single_data_group_is_converted_to_dup_and_the_kernel_repairs_one_copy_from_the_other() {
    let scratch = Scratch::new("relocate-convert-dup");
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

    let (new, extents) = {
        let mut fs = mount_rw(&image);
        let groups = data_groups(&fs);
        assert_eq!(groups.len(), 1, "one data group: {groups:?}");
        let old = groups[0];
        assert_eq!(
            ChunkProfile::from_type(old.flags).unwrap(),
            ChunkProfile::Single
        );
        assert!(old.used > 0, "the kernel's files are in the data group");
        let new = fs
            .relocate_block_group(old.start, Some(ChunkProfile::Dup))
            .expect("convert the populated single data group to DUP");
        let after = data_groups(&fs);
        assert_eq!(
            after
                .iter()
                .map(|g| (g.start, g.length, g.flags, g.used))
                .collect::<Vec<_>>(),
            vec![(new, old.length, bg::DATA | bg::DUP, old.used)],
            "one DUP data group, at the new address, of the old one's size and use"
        );
        let extents: Vec<(u64, u64)> = fs
            .block_group_extents(new)
            .unwrap()
            .iter()
            .map(|e| (e.start, e.len))
            .collect();
        assert!(!extents.is_empty(), "the files' extents moved");
        (new, extents)
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

    let stripes = stripe_offsets(&image, new);
    assert_eq!(stripes.len(), 2, "a DUP chunk has two stripes: {stripes:?}");
    assert_ne!(stripes[0], stripes[1]);
    assert_copies_agree(&image, new, &stripes, &extents, "after the conversion");

    // Corrupt the second copy of the largest extent's first sector: the
    // kernel can only repair it from the first copy.
    let &(at, _) = extents.iter().max_by_key(|e| e.1).unwrap();
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&image)
            .unwrap();
        file.seek(SeekFrom::Start(stripes[1] + (at - new))).unwrap();
        file.write_all(&[0xa5; 4096]).unwrap();
        file.sync_all().unwrap();
    }

    let scrubbed = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub the corrupted copy and report the profiles",
        "btrfs scrub start -B \"$MNT\"\n\
         btrfs filesystem df \"$MNT\"",
    );
    assert!(
        scrubbed.lines().any(|line| line.starts_with("Data, DUP")),
        "{scrubbed}"
    );
    assert!(
        !scrubbed
            .lines()
            .any(|line| line.starts_with("Data, single")),
        "{scrubbed}"
    );
    assert_copies_agree(&image, new, &stripes, &extents, "after the kernel's scrub");

    let read = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "read the converted files",
        MANIFEST,
    );
    assert_eq!(
        manifest(&read),
        before,
        "the kernel reads other bytes after the conversion:\n{read}"
    );
    assert_btrfs_check_clean(&image, "after the kernel scrubs the DUP group");
}
