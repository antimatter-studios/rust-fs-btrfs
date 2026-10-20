//! A data block group holding extents several file items name, relocated
//! into a newly allocated chunk, judged by what is not this crate (#495,
//! part of #398).
//!
//! The kernel makes the data: it mounts a fresh `mkfs.btrfs` volume and
//! writes files whose extents are named more than once: a whole-file
//! reflink (two inodes), a clone of a file's own first range into its
//! tail through `FICLONERANGE` (one inode, two reference offsets), and a
//! partial overwrite whose bookends name the old extent twice at one
//! reference offset. A SHA-256 manifest is taken from the kernel's own
//! reading before this crate touches the volume.
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
     for f in a a2 b c; do echo \"sum $f $(sha256sum \"$f\" | cut -d' ' -f1)\"; done";

fn manifest(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix("sum "))
        .filter_map(|l| l.split_once(' '))
        .map(|(name, sum)| (name.to_string(), sum.to_string()))
        .collect()
}

/// The kernel's files: `a` and its reflink `a2`, `b` overwritten in its
/// middle, and `c` with its first 64 KiB cloned over its last.
/// `0x4020940d` is `_IOW(0x94, 13, struct file_clone_range)`.
const FILL: &str = r#"cd "$MNT"
head -c 262144 /dev/urandom > a
head -c 524288 /dev/urandom > b
head -c 262144 /dev/urandom > c
sync
cp --reflink=always a a2
head -c 16384 /dev/urandom | dd of=b bs=4096 seek=16 conv=notrunc status=none
python3 - <<'PY'
import fcntl, os, struct
FICLONERANGE = 0x4020940d
s = os.open('c', os.O_RDONLY)
d = os.open('c', os.O_WRONLY)
fcntl.ioctl(d, FICLONERANGE, struct.pack('=qQQQ', s, 0, 65536, 196608))
os.close(s)
os.close(d)
PY
sync
"#;

#[test]
fn reflinked_and_partially_overwritten_extents_are_relocated_and_the_kernel_reads_them_back() {
    let scratch = Scratch::new("relocate-shared-data");
    let image = scratch.image();

    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "write the reflinked and overwritten files",
        &format!("{FILL}{MANIFEST}"),
    );
    let before = manifest(&wrote);
    assert_eq!(before.len(), 4, "the kernel reported:\n{wrote}");

    let (old, new) = {
        let mut fs = mount_rw(&image);
        let groups = data_groups(&fs);
        assert_eq!(groups.len(), 1, "one data group: {groups:?}");
        let old = groups[0];
        let extents = fs.block_group_extents(old.start).expect("enumerate");
        let inodes = |e: &fs_btrfs::balance::GroupExtent| {
            e.items
                .iter()
                .map(|i| i.inode)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        assert!(
            extents.iter().any(|e| inodes(e) > 1),
            "the reflink names an extent from two inodes: {extents:#?}"
        );
        assert!(
            extents.iter().any(|e| inodes(e) == 1
                && e.items.len() > 1
                && e.items.iter().any(|i| i.extent_offset > 0)),
            "one inode names an extent more than once, partway into it: {extents:#?}"
        );

        let new = fs
            .relocate_block_group(old.start, None)
            .expect("relocate a group of reflinked and overwritten extents");
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
        let moved = fs
            .block_group_extents(new)
            .expect("enumerate the new group");
        let named = |es: &[fs_btrfs::balance::GroupExtent]| {
            let mut v: Vec<_> = es.iter().map(|e| (e.len, e.items.clone())).collect();
            v.sort();
            v
        };
        assert_eq!(
            named(&moved),
            named(&extents),
            "every file item names its extent at the new address with its offset unchanged"
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
