//! A data block group holding more extents than one transaction moves,
//! relocated a batch at a time, and a refused relocation that leaves the
//! device untouched, judged by what is not this crate (#494, part of
//! #398).
//!
//! The kernel makes the data: it mounts a fresh `mkfs.btrfs` volume and
//! writes a hundred small files, each its own checksummed extent named by
//! one file item, so the group holds more extents than one batch. A
//! SHA-256 manifest is taken from the kernel's own reading before this
//! crate touches the volume.
//!
//! # The oracles
//!
//! After the relocation, `btrfs check --readonly --check-data-csum`
//! judges the metadata and every data sector against its digest, the
//! kernel reads every file of the manifest back with the same digests,
//! and a kernel scrub finds nothing. For the refusal, the evidence is the
//! image's raw bytes: a SHA-256 of the whole device before and after.

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// More files than one relocation transaction moves extents (64), each
/// written as one extent of its own.
const FILES: usize = 100;

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
            .args(["-q", "-f", "-s", "4096", "-n", "16384"])
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

/// `name digest` for every file in the volume's top directory, as the
/// kernel reads it.
const MANIFEST: &str = "cd \"$MNT\"\n\
     for f in *; do echo \"sum $f $(sha256sum \"$f\" | cut -d' ' -f1)\"; done";

fn manifest(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix("sum "))
        .filter_map(|l| l.split_once(' '))
        .map(|(name, sum)| (name.to_string(), sum.to_string()))
        .collect()
}

/// A SHA-256 of every byte of the image, read straight from the file.
fn image_digest(image: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(image).expect("open the image");
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).expect("read the image");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    format!("{:x}", hasher.finalize())
}

#[test]
fn a_data_group_of_more_extents_than_one_batch_is_relocated_and_the_kernel_reads_it_back() {
    let scratch = Scratch::new("relocate-data-batches");
    let image = scratch.image();

    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "write more files than one batch moves",
        &format!(
            "cd \"$MNT\"\n\
             for i in $(seq 1 {FILES}); do head -c 8192 /dev/urandom > f$i; sync; done\n\
             {MANIFEST}"
        ),
    );
    let before = manifest(&wrote);
    assert_eq!(before.len(), FILES, "the kernel reported:\n{wrote}");

    let new = {
        let mut fs = mount_rw(&image);
        let groups = data_groups(&fs);
        assert_eq!(groups.len(), 1, "one data group: {groups:?}");
        let old = groups[0];
        let extents = fs.block_group_extents(old.start).expect("enumerate");
        assert!(
            extents.len() > 64,
            "the group holds more extents than one batch: {}",
            extents.len()
        );
        let new = fs
            .relocate_block_group(old.start, None)
            .expect("relocate a data group of more extents than one batch");
        assert_ne!(new, old.start, "the group moved");
        let after = data_groups(&fs);
        assert_eq!(
            after
                .iter()
                .map(|g| (g.start, g.length, g.flags, g.used))
                .collect::<Vec<_>>(),
            vec![(new, old.length, old.flags, old.used)],
            "one data group, at the new address, of the old one's size, \
             profile and use"
        );
        new
    };

    assert_btrfs_check_clean(&image, "after the batched relocation");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests \
         after relocating into {new}:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let read = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "read the relocated files",
        MANIFEST,
    );
    assert_eq!(
        manifest(&read),
        before,
        "the kernel reads other bytes after the relocation:\n{read}"
    );

    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub the relocated group",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

#[test]
fn a_refused_relocation_leaves_the_device_byte_identical() {
    let scratch = Scratch::new("relocate-data-refused");
    let image = scratch.image();

    // A reflinked copy names its extent from two file items, which the
    // relocation refuses before anything is written.
    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "write a file and a reflinked copy of it",
        "cd \"$MNT\"\n\
         head -c 65536 /dev/urandom > a\n\
         sync\n\
         cp --reflink=always a b\n\
         sync\n\
         echo written",
    );
    assert!(wrote.contains("written"), "the kernel reported:\n{wrote}");

    let digest = image_digest(&image);
    {
        let mut fs = mount_rw(&image);
        let groups = data_groups(&fs);
        assert_eq!(groups.len(), 1, "one data group: {groups:?}");
        let refused = fs.relocate_block_group(groups[0].start, None);
        assert!(
            matches!(refused, Err(Error::UnsupportedFeature(_))),
            "a group with a shared extent is refused: {refused:?}"
        );
    }
    assert_eq!(
        image_digest(&image),
        digest,
        "the refused relocation changed the device"
    );
}
