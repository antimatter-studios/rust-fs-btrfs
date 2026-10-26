//! A kernel-written metadata group relocated, with its old range absent
//! from btrfs-progs' tree dump and every file read back by the kernel (#580).

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::chunk::{block_group, ChunkProfile};
use fs_btrfs::fs::Filesystem;
use fs_btrfs::Error;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str, profile: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let scratch = Self(dir);
        std::fs::File::create(scratch.image())
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args([
                "-q", "-f", "-m", profile, "-d", "single", "-s", "4096", "-n", "16384",
            ])
            .arg(scratch.image())
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        scratch
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

fn metadata_group(fs: &Filesystem) -> BlockGroup {
    fs.block_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.holds_metadata() && g.flags & block_group::SYSTEM == 0)
        .expect("a metadata group")
}

const MANIFEST: &str = "cd \"$MNT\"\n\
    find . -type f -exec sha256sum {} + | sort";

fn manifest(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once("  "))
        .filter(|(sum, _)| sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|(sum, name)| (name.to_string(), sum.to_string()))
        .collect()
}

fn digest(image: &Path) -> Vec<u8> {
    let mut file = std::fs::File::open(image).unwrap();
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    hash.finalize().to_vec()
}

#[test]
fn a_kernel_written_metadata_group_is_relocated_and_every_file_survives() {
    let scratch = Scratch::new("relocate-metadata-group", "single");
    let image = scratch.image();
    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "populate metadata before relocation",
        &format!(
            "cd \"$MNT\"\n\
             mkdir files\n\
             for i in $(seq 1 192); do\n\
                 head -c 8192 /dev/urandom > \"files/f$i\"\n\
             done\n\
             btrfs subvolume create sub >/dev/null\n\
             head -c 65536 /dev/urandom > sub/payload\n\
             sync\n\
             {MANIFEST}"
        ),
    );
    let before = manifest(&wrote);
    assert_eq!(before.len(), 193, "the kernel manifest: {wrote}");

    let (old, new) = {
        let mut fs = mount_rw(&image);
        let old = metadata_group(&fs);
        assert!(old.used > 0, "the kernel populated metadata");
        let new = fs
            .relocate_block_group(old.start, None)
            .expect("relocate the populated metadata group");
        assert_ne!(new, old.start);
        let groups = fs.block_groups().unwrap();
        assert!(groups.iter().all(|g| g.start != old.start));
        let replacement = groups.iter().find(|g| g.start == new).unwrap();
        assert_eq!(replacement.flags, old.flags);
        assert_eq!(replacement.length, old.length);
        assert!(replacement.used > 0, "tree blocks moved into new metadata");
        (old, new)
    };

    assert_btrfs_check_clean(&image, "after metadata relocation");
    let dumped = oracle("btrfs")
        .args(["inspect-internal", "dump-tree"])
        .arg(&image)
        .output();
    assert!(dumped.status.success(), "dump-tree failed: {dumped:?}");
    let tree_dump = String::from_utf8_lossy(&dumped.stdout);
    let inside: Vec<u64> = tree_dump
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            match words.next() {
                Some("leaf" | "node") => words.next()?.parse().ok(),
                _ => None,
            }
        })
        .filter(|at| (old.start..old.end()).contains(at))
        .collect();
    assert!(
        inside.is_empty(),
        "tree blocks remain in old group: {inside:?}"
    );
    let chunks = dump_tree(&image, "chunk");
    assert!(chunks.contains(&format!("CHUNK_ITEM {new})")));
    assert!(!chunks.contains(&format!("CHUNK_ITEM {})", old.start)));
    let extents = dump_tree(&image, "extent");
    assert!(!extents.contains(&format!("key ({} BLOCK_GROUP_ITEM ", old.start)));

    let read = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "read every file after metadata relocation",
        MANIFEST,
    );
    assert_eq!(
        manifest(&read),
        before,
        "kernel reads different files: {read}"
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub and write after metadata relocation",
        "btrfs scrub start -B \"$MNT\" >/dev/null\n\
         echo after > \"$MNT/after\"\n\
         sync",
    );
    assert_btrfs_check_clean(&image, "after the kernel writes into relocated metadata");
}

/// A kernel-written single metadata group converted to DUP and back
/// (#588): `btrfs filesystem df` shows each profile in turn, `btrfs check`
/// is clean after each, and the kernel reads every file it wrote before.
#[test]
fn a_kernel_written_metadata_group_converts_to_dup_and_back() {
    let scratch = Scratch::new("relocate-metadata-convert", "single");
    let image = scratch.image();
    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "populate metadata before conversion",
        &format!(
            "cd \"$MNT\"\n\
             mkdir files\n\
             for i in $(seq 1 96); do\n\
                 head -c 8192 /dev/urandom > \"files/f$i\"\n\
             done\n\
             sync\n\
             {MANIFEST}"
        ),
    );
    let before = manifest(&wrote);
    assert_eq!(before.len(), 96, "the kernel manifest: {wrote}");

    let mut start = metadata_group(&mount_rw(&image)).start;
    for (to, shown, gone) in [
        (ChunkProfile::Dup, "Metadata, DUP", "Metadata, single"),
        (ChunkProfile::Single, "Metadata, single", "Metadata, DUP"),
    ] {
        start = {
            let mut fs = mount_rw(&image);
            let new = fs
                .relocate_block_group(start, Some(to))
                .unwrap_or_else(|e| panic!("convert metadata to {}: {e}", to.name()));
            let replacement = fs
                .block_groups()
                .unwrap()
                .into_iter()
                .find(|g| g.start == new)
                .unwrap();
            assert_eq!(ChunkProfile::from_type(replacement.flags).unwrap(), to);
            new
        };
        assert_btrfs_check_clean(
            &image,
            &format!("after converting metadata to {}", to.name()),
        );
        let read = guest_kernel_read_ok(
            &image.to_string_lossy(),
            &format!("read every file after converting metadata to {}", to.name()),
            &format!("{MANIFEST}\nbtrfs filesystem df \"$MNT\""),
        );
        assert!(
            read.lines().any(|line| line.starts_with(shown)),
            "`btrfs filesystem df` shows no {shown}: {read}"
        );
        assert!(
            !read.lines().any(|line| line.starts_with(gone)),
            "`btrfs filesystem df` still shows {gone}: {read}"
        );
        assert_eq!(
            manifest(&read),
            before,
            "kernel reads different files: {read}"
        );
    }
}

#[test]
fn unsupported_metadata_profiles_are_refused_before_allocation() {
    for (name, profile, conversion, reason) in [
        ("dup", "dup", None, "only single"),
        (
            "conversion",
            "single",
            Some(ChunkProfile::Raid1),
            "converting",
        ),
    ] {
        let scratch = Scratch::new(&format!("relocate-metadata-{name}"), profile);
        let image = scratch.image();
        let before = digest(&image);
        let mut fs = mount_rw(&image);
        let group = metadata_group(&fs);
        let error = fs
            .relocate_block_group(group.start, conversion)
            .unwrap_err();
        assert!(
            matches!(&error, Error::UnsupportedFeature(why) if why.contains(reason)),
            "expected {reason}: {error}"
        );
        drop(fs);
        assert_eq!(digest(&image), before, "profile refusal wrote to the image");
    }
}
