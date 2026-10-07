//! The default subvolume, chosen by this crate, judged by btrfs-progs and
//! the kernel (#267).
//!
//! On a copy of the kernel-made subvolume fixture, this crate makes `sub`
//! the default. `btrfs check` must find the volume clean,
//! `dump-super` must show the `DEFAULT_SUBVOL` feature, `btrfs subvolume
//! get-default` must name `sub`, and a plain kernel mount -- no `subvol=`
//! -- must show `sub`'s contents rather than the top level's. Then the
//! top level is made the default again, and the plain mount shows it.

use std::sync::Arc;

use fs_btrfs::fs::Filesystem;
use fs_btrfs::superblock::incompat::DEFAULT_SUBVOL;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_super, fixture, guest_kernel_probe, guest_kernel_read_ok,
};
use fs_core::{BlockDevice, FileDevice};

const LOOK: &str = r#"
btrfs subvolume get-default "$MNT" | awk '{print "default " $2}'
ls -1 "$MNT" | LC_ALL=C sort | paste -sd' ' -
"#;

fn mount_rw(image: &std::path::Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open rw"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw")
}

#[test]
fn the_default_subvolume_is_what_a_plain_kernel_mount_shows() {
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "subvol-default-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("btrfs-subvol.img");
    std::fs::copy(fixture("btrfs-subvol.img"), &image).expect("copy the fixture");
    let image_str = image.to_str().expect("a UTF-8 scratch path").to_string();

    let sub = {
        let mut fs = mount_rw(&image);
        let sub = fs
            .subvolumes()
            .unwrap()
            .into_iter()
            .find(|s| s.path == "sub")
            .expect("the fixture has sub")
            .id;
        let before = fs.superblock().generation;
        fs.set_default_subvolume(sub).expect("make sub the default");
        assert_eq!(fs.superblock().generation, before + 1, "one transaction");
        assert_ne!(fs.superblock().incompat_flags & DEFAULT_SUBVOL, 0);
        fs.set_default_subvolume(sub).expect("again");
        assert_eq!(fs.superblock().generation, before + 1, "no-op committed");
        sub
    };

    assert_btrfs_check_clean(&image, "after set-default");
    assert!(
        dump_super(&image).contains("DEFAULT_SUBVOL"),
        "dump-super does not list the DEFAULT_SUBVOL feature"
    );
    let probe = guest_kernel_probe(&image_str, "default");
    assert!(
        probe.complaints.is_empty(),
        "the kernel complained: {:?}",
        probe.complaints
    );
    let seen = guest_kernel_read_ok(&image_str, "default is sub", LOOK);
    assert_eq!(
        seen.lines().map(str::trim).collect::<Vec<_>>(),
        [format!("default {sub}").as_str(), "after.txt b.txt inner"],
        "a plain mount after making sub the default:\n{seen}"
    );

    {
        let mut fs = mount_rw(&image);
        fs.set_default_subvolume(5)
            .expect("make the top level the default");
    }
    assert_btrfs_check_clean(&image, "after set-default back to the top level");
    let seen = guest_kernel_read_ok(&image_str, "default is the top level", LOOK);
    assert_eq!(
        seen.lines().map(str::trim).collect::<Vec<_>>(),
        ["default 5", "rosnap snap sub top"],
        "a plain mount after making the top level the default again:\n{seen}"
    );
}
