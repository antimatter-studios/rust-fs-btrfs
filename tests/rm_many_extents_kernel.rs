//! `fs.btrfs rm` of a file with enough data extents that their records
//! fill whole extent tree leaves (#724).
//!
//! Removing such a file empties those leaves, and an emptied leaf leaves
//! its tree without being rendered. The extent records in it are released
//! all the same, so they count as released.
//!
//! # The oracles
//!
//! `mkfs.btrfs` makes the volume and the kernel, in the harness VM, writes
//! the file, one block in every two so no two blocks share an extent. After
//! this crate removes the file, `btrfs check --readonly` grades the volume
//! and the kernel mounts it, digests the file that stayed and lists the
//! names it finds.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, sha256_hex,
};
use std::path::PathBuf;

/// Enough extents that their records fill several 16 KiB leaves: each
/// takes about 78 bytes of one, so a leaf holds about 200.
const EXTENTS: usize = 1500;

#[test]
fn a_file_whose_extent_records_fill_whole_leaves_is_removed() {
    let dir = PathBuf::from(fs_btrfs_test_support::temp_path!("rm-many-extents"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(1 << 30))
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
    let image_str = image.to_str().unwrap();
    guest_kernel_write_ok(
        image_str,
        "the kernel writes a file in many extents",
        &format!(
            "printf 'stays\\n' > \"$MNT/keep\"\n\
             i=0\n\
             while [ $i -lt {EXTENTS} ]; do\n\
             dd if=/dev/urandom of=\"$MNT/many\" bs=4k count=1 seek=$((i*2)) conv=notrunc \
             status=none\n\
             i=$((i+1))\n\
             done\n\
             sync"
        ),
    );

    ok(tool("fs.btrfs").arg(&image).args(["rm", "/many"]));

    assert_btrfs_check_clean(&image, "after removing the file");
    let out = guest_kernel_read_ok(
        image_str,
        "the kernel reads the volume back",
        "echo \"SHA $(sha256sum \"$MNT/keep\" | cut -d' ' -f1)\"\n\
         (cd \"$MNT\" && find . -mindepth 1 | sort | sed 's/^\\./NAME /')\n",
    );
    assert!(
        out.contains(&format!("SHA {}", sha256_hex(b"stays\n"))),
        "the file that stayed changed: {out}"
    );
    let names: Vec<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix("NAME "))
        .collect();
    assert_eq!(names, ["/keep"], "names the kernel lists: {out}");

    let _ = std::fs::remove_dir_all(&dir);
}
