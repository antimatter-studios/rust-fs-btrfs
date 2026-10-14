//! A volume that keeps its free space in the v1 space cache, made and
//! filled by the kernel, is read, checked and written, and judged by what
//! is not this crate (#270).
//!
//! Without the free-space tree (`mkfs.btrfs -R ^free-space-tree`) the
//! kernel mounts with `space_cache=v1`: each block group's free space is
//! saved in a hidden file of the root tree, an inode with `EXTENT_DATA`
//! items and data extents of its own, and the superblock's
//! `cache_generation` records the transaction that wrote them.
//!
//! - Reading never looks at the cache, but `fsck.btrfs` walks the extent
//!   tree, and the cache files' data extents are referenced from the root
//!   tree rather than from a subvolume.
//! - This driver keeps no v1 cache. A commit moves `generation` and
//!   leaves `cache_generation` behind, which is how the kernel knows to
//!   discard every cache and rebuild it rather than trust one that does
//!   not know about the extents written since. If it trusted it, it
//!   would hand out space this driver has used, and the next write would
//!   land on top of a file.
//!
//! So the volume is written here, `btrfs check` grades it, and the kernel
//! then reads it back and writes enough after it to reuse that space if
//! it believed the old cache, before `btrfs check` -- which checks the
//! caches the kernel wrote this time -- grades it again.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_super, dump_tree, guest_kernel_write_ok, oracle, sha256_hex,
    temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A scratch volume, removed when it drops -- including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    /// A 256 MiB volume without the free-space tree, filled by the kernel
    /// under the v1 space cache.
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-R", "^free-space-tree"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(
            &image.to_string_lossy(),
            name,
            "cd \"$MNT\"\n\
             mkdir dir\n\
             for i in 1 2 3 4 5; do echo \"v1 cache file $i\" > dir/file-$i.txt; done\n\
             head -c 8388608 /dev/urandom > big.bin\n\
             head -c 3000000 /dev/urandom > middle.bin\n\
             head -c 1048576 /dev/urandom > ours.bin\n\
             sync",
        );
        let scratch = Scratch(dir);
        scratch.assert_kernel_kept_a_v1_cache("as the kernel left it");
        scratch
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }

    /// The precondition, from btrfs-progs: no free-space tree, a cache
    /// header in the root tree, and a `cache_generation` that is the
    /// generation the kernel committed.
    fn assert_kernel_kept_a_v1_cache(&self, when: &str) {
        let image = self.image();
        let (generation, cache_generation, compat_ro) = super_fields(&image);
        assert_eq!(
            compat_ro & 1,
            0,
            "{when}: the volume has a free-space tree (compat_ro {compat_ro:#x})"
        );
        assert_eq!(
            cache_generation, generation,
            "{when}: the kernel did not save a v1 cache at its last commit"
        );
        assert!(
            dump_tree(&image, "1").contains("FREE_SPACE UNTYPED"),
            "{when}: the root tree holds no v1 space cache header"
        );
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `(generation, cache_generation, compat_ro_flags)` from
/// `btrfs inspect-internal dump-super`.
fn super_fields(image: &Path) -> (u64, u64, u64) {
    let dump = dump_super(image);
    let field = |name: &str| -> u64 {
        let value = dump
            .lines()
            .find_map(|l| {
                let mut f = l.split_whitespace();
                (f.next() == Some(name)).then(|| f.next().unwrap_or("").to_string())
            })
            .unwrap_or_else(|| panic!("dump-super has no {name}:\n{dump}"));
        match value.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => value.parse(),
        }
        .unwrap_or_else(|e| panic!("dump-super's {name} {value:?}: {e}"))
    };
    (
        field("generation"),
        field("cache_generation"),
        field("compat_ro_flags"),
    )
}

fn mount(image: &Path) -> Filesystem {
    let dev = FileDevice::open(image).expect("open");
    Filesystem::mount(Arc::new(dev)).expect("a v1 space cache volume mounts")
}

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open read-write"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>)
        .expect("a v1 space cache volume mounts read-write")
}

fn assert_fsck_clean(image: &Path, when: &str) {
    let report = fs_btrfs::check::check(&mount(image));
    let findings: Vec<String> = report.findings.iter().map(|f| f.what.clone()).collect();
    assert!(findings.is_empty(), "{when}: {findings:#?}");
}

/// Bytes that are not all one value, so a misplaced block shows.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ (i >> 12) as u8)
        .collect()
}

#[test]
fn fsck_finds_the_volume_clean_as_btrfs_check_does() {
    let scratch = Scratch::new("space-cache-v1-fsck");
    assert_btrfs_check_clean(&scratch.image(), "as the kernel left it");
    assert_fsck_clean(&scratch.image(), "as the kernel left it");
}

#[test]
fn a_write_leaves_the_cache_for_the_kernel_to_rebuild() {
    let scratch = Scratch::new("space-cache-v1-write");
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    let before = fs.read_path("/big.bin").expect("big.bin");
    // Each overwrite is copy-on-write: the new bytes go to an extent in
    // space the old cache recorded as free, and the extent they replace
    // is freed. middle.bin is removed, which frees a run in the middle
    // of what the cache saw.
    let ours = pattern(1 << 20, 7);
    let ino = fs.lookup_path("/ours.bin").unwrap().ino;
    fs.write(ino, 0, &ours).expect("overwrite ours.bin");
    let mut big = before.clone();
    big[1 << 20..(1 << 20) + 65536].copy_from_slice(&pattern(65536, 99));
    let big_ino = fs.lookup_path("/big.bin").unwrap().ino;
    fs.write(big_ino, 1 << 20, &big[1 << 20..(1 << 20) + 65536])
        .expect("overwrite part of big.bin");
    fs.unlink(256, b"middle.bin").expect("remove middle.bin");
    let sb = fs.superblock().clone();
    drop(fs);
    assert_ne!(
        sb.cache_generation, sb.generation,
        "a commit by this driver must leave cache_generation behind, so the kernel \
         discards a cache that does not know about its extents"
    );
    assert_btrfs_check_clean(&image, "after this driver's writes");
    assert_fsck_clean(&image, "after this driver's writes");

    // The kernel: what this driver wrote reads back, and 96 MiB written
    // after it lands somewhere else. A kernel that trusted the old cache
    // would take ours.bin's space as free.
    let seen = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel after this driver",
        "cd \"$MNT\"\n\
         printf 'ours %s\\n' \"$(sha256sum ours.bin | cut -d' ' -f1)\"\n\
         printf 'big %s\\n' \"$(sha256sum big.bin | cut -d' ' -f1)\"\n\
         test ! -e middle.bin\n\
         for i in 1 2 3 4 5 6; do head -c 16777216 /dev/urandom > after-$i.bin; sync; done\n\
         printf 'ours-after %s\\n' \"$(sha256sum ours.bin | cut -d' ' -f1)\"",
    );
    let want_ours = sha256_hex(&ours);
    for line in [
        format!("ours {want_ours}"),
        format!("big {}", sha256_hex(&big)),
        format!("ours-after {want_ours}"),
    ] {
        assert!(
            seen.lines().any(|l| l == line),
            "want {line:?}; the kernel said:\n{seen}"
        );
    }

    // The kernel saved a fresh cache at its last commit, and btrfs check
    // checks that one against the extent tree.
    scratch.assert_kernel_kept_a_v1_cache("after the kernel wrote again");
    assert_btrfs_check_clean(&image, "after the kernel wrote again");
    assert_fsck_clean(&image, "after the kernel wrote again");
    let fs = mount(&image);
    assert_eq!(fs.read_path("/ours.bin").unwrap(), ours);
    assert_eq!(fs.read_path("/big.bin").unwrap(), big);
}
