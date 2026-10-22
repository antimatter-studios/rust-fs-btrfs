//! A data block group holding preallocated extents and the extents of a
//! `nodatasum` file, relocated into a newly allocated chunk, judged by
//! what is not this crate (#496, part of #398).
//!
//! The kernel makes the data: it mounts a fresh `mkfs.btrfs` volume and
//! writes an ordinary file, a file preallocated with `fallocate` and never
//! written, a preallocated file written in its middle (one extent named by
//! a written item with checksums and by unwritten items without), and a
//! file marked `NOCOW`, which the kernel stores without checksums. A
//! SHA-256 manifest is taken from the kernel's own reading before this
//! crate touches the volume.
//!
//! # The oracles
//!
//! After the relocation, `btrfs check --readonly --check-data-csum`
//! judges the metadata and every data sector against its digest, so a
//! checksum invented for a sector the kernel left without one is caught;
//! the kernel reads every file of the manifest back with the same
//! digests, and a kernel scrub finds nothing. The kernel then writes into
//! the preallocated ranges, the written middle and the `NOCOW` file,
//! where it writes in place only when the extent tree names each extent
//! where it now is, and `btrfs check` judges the volume again.

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
     for f in a p q n; do echo \"sum $f $(sha256sum \"$f\" | cut -d' ' -f1)\"; done";

fn manifest(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix("sum "))
        .filter_map(|l| l.split_once(' '))
        .map(|(name, sum)| (name.to_string(), sum.to_string()))
        .collect()
}

/// The kernel's files: `a` written whole, `p` preallocated and never
/// written, `q` preallocated and written in its middle, and `n` marked
/// `NOCOW` while empty and then written. `0x40086602` is
/// `FS_IOC_SETFLAGS`, and `0x00800000` is `FS_NOCOW_FL`.
const FILL: &str = r#"cd "$MNT"
head -c 262144 /dev/urandom > a
fallocate -l 262144 p
fallocate -l 262144 q
head -c 16384 /dev/urandom | dd of=q bs=4096 seek=16 conv=notrunc status=none
touch n
python3 - <<'PY'
import fcntl, os, struct
FS_IOC_SETFLAGS = 0x40086602
FS_NOCOW_FL = 0x00800000
fd = os.open('n', os.O_RDONLY)
fcntl.ioctl(fd, FS_IOC_SETFLAGS, struct.pack('=l', FS_NOCOW_FL))
os.close(fd)
PY
head -c 262144 /dev/urandom | dd of=n bs=4096 conv=notrunc status=none
sync
"#;

/// The kernel's writes into the relocated files: into `p`'s unwritten
/// range, over `q`'s written middle and its unwritten tail, and over and
/// past the end of the `NOCOW` file `n`.
const WRITE_AFTER: &str = r#"cd "$MNT"
head -c 8192 /dev/urandom | dd of=p bs=4096 seek=4 conv=notrunc status=none
head -c 8192 /dev/urandom | dd of=q bs=4096 seek=17 conv=notrunc status=none
head -c 8192 /dev/urandom | dd of=q bs=4096 seek=60 conv=notrunc status=none
head -c 8192 /dev/urandom | dd of=n bs=4096 seek=8 conv=notrunc status=none
head -c 8192 /dev/urandom >> n
sync
"#;

#[test]
fn preallocated_and_nodatasum_extents_are_relocated_and_the_kernel_writes_them_afterwards() {
    let scratch = Scratch::new("relocate-prealloc-nodatasum");
    let image = scratch.image();

    let wrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "write the preallocated and nodatasum files",
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
        let inodes: std::collections::BTreeSet<u64> = extents
            .iter()
            .flat_map(|e| e.items.iter().map(|i| i.inode))
            .collect();
        assert_eq!(
            inodes.len(),
            4,
            "every file's extents are in the group: {extents:#?}"
        );
        assert!(
            extents.iter().any(|e| e.items.len() >= 3),
            "the preallocated file written in its middle names its extent three times: \
             {extents:#?}"
        );

        let new = fs
            .relocate_block_group(old.start, None)
            .expect("relocate a group of preallocated and nodatasum extents");
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

    let check_data_csum = |when: &str| {
        let checked = oracle("btrfs")
            .args(["check", "--readonly", "--check-data-csum"])
            .arg(&image)
            .output();
        assert_eq!(
            checked.status.code(),
            Some(0),
            "btrfs check --check-data-csum {when} finds data that disagrees with its \
             digests, or digests the kernel never wrote:\n{}{}",
            String::from_utf8_lossy(&checked.stdout),
            String::from_utf8_lossy(&checked.stderr)
        );
    };
    assert_btrfs_check_clean(&image, "after the relocation");
    check_data_csum("after the relocation");

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

    let rewrote = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "write into the relocated files",
        &format!("{WRITE_AFTER}{MANIFEST}"),
    );
    let written = manifest(&rewrote);
    assert_eq!(written.len(), 4, "the kernel reported:\n{rewrote}");
    assert_eq!(written["a"], before["a"], "the untouched file is unchanged");
    for f in ["p", "q", "n"] {
        assert_ne!(written[f], before[f], "the kernel's write to {f} took");
    }
    assert_btrfs_check_clean(&image, "after the kernel wrote to the relocated files");
    check_data_csum("after the kernel wrote to the relocated files");

    let reread = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "read the files the kernel rewrote",
        MANIFEST,
    );
    assert_eq!(
        manifest(&reread),
        written,
        "the kernel reads back other bytes than it wrote:\n{reread}"
    );
}
