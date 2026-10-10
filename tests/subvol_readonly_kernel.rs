//! A subvolume's read-only flag, set and cleared by this crate, judged by
//! btrfs-progs and the kernel (#267, first slice).
//!
//! The kernel-made subvolume fixture has a writable subvolume `sub` and a
//! read-only snapshot `rosnap`. This crate makes `sub` read-only and
//! `rosnap` writable, each as one committed transaction. Then `btrfs
//! check` must find the volume clean, `btrfs property get` must report the
//! new flags, and the kernel must refuse a file created in `sub` and
//! accept one created in `rosnap` -- the flag is what it enforces, not
//! only what it prints -- without logging a complaint while mounting it.

use std::sync::Arc;

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, fixture, guest_kernel_probe, guest_kernel_write_ok,
};
use fs_core::{BlockDevice, FileDevice};

fn id_of(fs: &Filesystem, name: &str) -> (u64, bool) {
    fs.subvolumes()
        .expect("list the subvolumes")
        .into_iter()
        .find(|s| s.path == name)
        .map(|s| (s.id, s.read_only))
        .unwrap_or_else(|| panic!("the subvolume fixture has no {name}"))
}

#[test]
fn the_read_only_flag_is_set_and_cleared_and_the_kernel_enforces_it() {
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "subvol-ro-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("btrfs-subvol.img");
    std::fs::copy(fixture("btrfs-subvol.img"), &image).expect("copy the fixture");

    {
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open rw"));
        let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
        let (sub, sub_ro) = id_of(&fs, "sub");
        let (rosnap, rosnap_ro) = id_of(&fs, "rosnap");
        assert!(
            !sub_ro && rosnap_ro,
            "the fixture is not the shape this test expects"
        );

        let before = fs.superblock().generation;
        fs.set_subvolume_read_only(sub, true)
            .expect("make sub read-only");
        assert_eq!(fs.superblock().generation, before + 1, "one transaction");
        fs.set_subvolume_read_only(rosnap, false)
            .expect("make rosnap writable");
        assert_eq!(fs.superblock().generation, before + 2, "one transaction");

        // Already so: nothing to commit.
        fs.set_subvolume_read_only(sub, true).expect("again");
        assert_eq!(fs.superblock().generation, before + 2, "no-op committed");

        assert!(id_of(&fs, "sub").1, "sub reads back writable");
        assert!(!id_of(&fs, "rosnap").1, "rosnap reads back read-only");
    }

    // A read-only mount writes nothing.
    {
        let dev = Arc::new(FileDevice::open(&image).expect("open"));
        let mut fs = Filesystem::mount(dev).expect("mount");
        let (sub, _) = id_of(&fs, "sub");
        assert_eq!(fs.set_subvolume_read_only(sub, false), Err(Error::ReadOnly));
    }

    assert_btrfs_check_clean(&image, "after the read-only flags changed");

    let image = image.to_str().expect("a UTF-8 scratch path").to_string();
    let probe = guest_kernel_probe(&image, "flags");
    assert!(
        probe.complaints.is_empty(),
        "the kernel complained mounting the volume: {:?}",
        probe.complaints
    );
    let out = guest_kernel_write_ok(
        &image,
        "the read-only flags",
        r#"
btrfs property get "$MNT/sub" ro
btrfs property get "$MNT/rosnap" ro
if touch "$MNT/sub/refused.txt" 2>/dev/null; then echo sub=written; else echo sub=refused; fi
touch "$MNT/rosnap/written.txt" && echo rosnap=written
"#,
    );
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    assert_eq!(
        lines,
        ["ro=true", "ro=false", "sub=refused", "rosnap=written"],
        "the kernel's view of the flags:\n{out}"
    );
}
