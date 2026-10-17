//! A data block group holding plain checksummed extents, relocated into
//! a newly allocated chunk in one transaction, judged by what is not this
//! crate (#493, part of #398).
//!
//! The kernel makes the data: it mounts a fresh `mkfs.btrfs` volume and
//! writes a few files, each in two appends, so every extent is named by
//! one file item, carries data checksums, and is neither compressed nor
//! shared. A SHA-256 manifest is taken from the kernel's own reading
//! before this crate touches the volume.
//!
//! # The oracles
//!
//! After the relocation, `btrfs check --readonly --check-data-csum`
//! judges the metadata and every data sector against its digest, the
//! kernel reads every file of the manifest back with the same digests,
//! and a kernel scrub finds nothing.

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeMap;
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

#[test]
fn a_data_group_of_plain_checksummed_extents_is_relocated_and_the_kernel_reads_it_back() {
    let scratch = Scratch::new("relocate-data-group");
    let image = scratch.image();

    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "write the files to relocate",
        &format!(
            "cd \"$MNT\"\n\
             head -c 65536 /dev/urandom > a\n\
             head -c 200704 /dev/urandom > b\n\
             head -c 12288 /dev/urandom > c\n\
             head -c 1048576 /dev/urandom > d\n\
             sync\n\
             head -c 40960 /dev/urandom >> a\n\
             head -c 8192 /dev/urandom >> b\n\
             head -c 4096 /dev/urandom >> c\n\
             head -c 131072 /dev/urandom >> d\n\
             sync\n\
             {MANIFEST}"
        ),
    );
    let before = manifest(&wrote);
    assert_eq!(before.len(), 4, "the kernel reported:\n{wrote}");

    let (old, new) = {
        let mut fs = mount_rw(&image);
        let groups = data_groups(&fs);
        assert_eq!(groups.len(), 1, "one data group: {groups:?}");
        let old = groups[0];
        assert!(old.used > 0, "the kernel's files are in the data group");
        let new = fs
            .relocate_block_group(old.start, None)
            .expect("relocate the populated data group");
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
        (old.start, new)
    };
    assert_ne!(old, new);

    assert_btrfs_check_clean(&image, "after the relocation");
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
