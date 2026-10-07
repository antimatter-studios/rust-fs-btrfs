//! The default subvolume, chosen by this crate, judged by btrfs-progs and
//! the kernel (#267).
//!
//! On a copy of the kernel-made subvolume fixture, this crate makes `sub`
//! the default. `btrfs check` must find the volume clean,
//! `dump-super` must show the `DEFAULT_SUBVOL` feature, `btrfs subvolume
//! get-default` must name `sub`, and a plain kernel mount -- no `subvol=`
//! -- must show `sub`'s contents rather than the top level's. Then the
//! top level is made the default again, and the plain mount shows it.
//!
//! At each step this crate's own reading of the default
//! (`default_subvolume`) must be the one `get-default` names, and
//! `subvolume_at` must give each subvolume the id `btrfs inspect-internal
//! rootid` gives the same path.

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

/// The top level mounted: each subvolume's id by its path, as the kernel
/// gives it.
const ROOT_IDS: &str = r#"
for p in sub sub/inner snap rosnap; do echo "$p $(btrfs inspect-internal rootid "$MNT/$p")"; done
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
        assert_eq!(fs.default_subvolume().unwrap(), 5, "the fixture's default");
        let before = fs.superblock().generation;
        fs.set_default_subvolume(sub).expect("make sub the default");
        assert_eq!(fs.default_subvolume().unwrap(), sub, "read back");
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
        assert_eq!(fs.default_subvolume().unwrap(), sub, "on disk");
        fs.set_default_subvolume(5)
            .expect("make the top level the default");
        assert_eq!(fs.default_subvolume().unwrap(), 5, "read back");
    }
    assert_btrfs_check_clean(&image, "after set-default back to the top level");
    let ours: Vec<String> = {
        let fs = mount_rw(&image);
        ["sub", "sub/inner", "snap", "rosnap"]
            .iter()
            .map(|p| {
                let id = fs
                    .subvolume_at(format!("/{p}").as_bytes())
                    .unwrap_or_else(|e| panic!("subvolume_at /{p}: {e}"));
                format!("{p} {id}")
            })
            .collect()
    };
    let seen = guest_kernel_read_ok(
        &image_str,
        "default is the top level",
        &format!("{LOOK}{ROOT_IDS}"),
    );
    let mut expected = vec!["default 5".to_string(), "rosnap snap sub top".to_string()];
    expected.extend(ours);
    assert_eq!(
        seen.lines().map(str::trim).collect::<Vec<_>>(),
        expected,
        "a plain mount after making the top level the default again, and each subvolume's id:\n{seen}"
    );
}
