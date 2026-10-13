//! The leaves a change splits, judged by `btrfs check` and read by the
//! kernel (#342). `tests/leaf_split.rs` shows this crate reads back what
//! it wrote; that is its own reading of its own blocks. Here the same
//! two hundred names, which split a directory's leaf, grow its tree a
//! level and split again under the new node, go to an image file, which
//! btrfs-progs must find clean and the kernel must list in full.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::inode::FIRST_FREE_OBJECTID;
use fs_btrfs_test_support::{assert_btrfs_check_clean, guest_kernel_read_ok, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::path::PathBuf;
use std::sync::Arc;

const FILES: usize = 200;

/// A scratch image, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn split_leaves_are_clean_to_btrfs_check_and_read_by_the_kernel() {
    let dir = Scratch(PathBuf::from(temp_path!("leaf-split-oracle")));
    std::fs::create_dir_all(&dir.0).unwrap();
    let image = dir.0.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .unwrap();

    let dev = Arc::new(FileDevice::open_rw(&image).expect("open the image"));
    let opts = fs_btrfs::mkfs::Options {
        nodesize: 4096,
        ..fs_btrfs::mkfs::Options::default()
    };
    fs_btrfs::mkfs::format(dev.as_ref(), &opts).expect("mkfs");
    let mut fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    for i in 0..FILES {
        let name = format!("a-file-with-a-longish-name-{i:04}");
        fs.create(FIRST_FREE_OBJECTID, name.as_bytes(), 0o644, 0, 0)
            .unwrap_or_else(|e| panic!("create {} of {FILES}: {e}", i + 1));
    }
    drop(fs);
    drop(dev);

    assert_btrfs_check_clean(&image, "two hundred names, their leaves split");
    let listed = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "leaf split",
        "ls \"$MNT\" | grep -c '^a-file-with-a-longish-name-'",
    );
    assert_eq!(
        listed.trim(),
        FILES.to_string(),
        "the kernel lists {listed} of the {FILES} names"
    );
}
