//! A subvolume deleted by this crate, judged by btrfs-progs and the
//! kernel (#267).
//!
//! On a copy of the kernel-made subvolume fixture, this crate deletes the
//! read-only snapshot `rosnap` and the subvolume `top/nested`, which sits
//! in an ordinary directory, each as one committed transaction, and is
//! refused `sub`, which holds the subvolume `inner`. A delete is lazy, as
//! the kernel's is: the names and references go, and the tree is left
//! for the kernel's cleaner, marked by an orphan item. Then:
//!
//! - `btrfs check` must find the volume clean, and the root tree must
//!   hold an orphan item for each deleted subvolume;
//! - the kernel must mount the volume, leave both out of
//!   `btrfs subvolume list`, and finish dropping them when
//!   `btrfs subvolume sync` waits for its cleaner;
//! - after the cleaner the volume must still be clean, the root tree must
//!   hold neither tree's root item nor its orphan item, and this crate
//!   must list what the kernel lists.

use std::sync::Arc;

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{assert_btrfs_check_clean, dump_tree, fixture, guest_kernel_write_ok};
use fs_core::{BlockDevice, FileDevice};

/// The id of the subvolume listed at `path`, from this crate's listing.
fn id_at(fs: &Filesystem, path: &str) -> Option<u64> {
    fs.subvolumes()
        .expect("list the subvolumes")
        .into_iter()
        .find(|s| s.path == path)
        .map(|s| s.id)
}

#[test]
fn a_deleted_subvolume_is_one_the_kernel_drops() {
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "subvol-delete-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("btrfs-subvol.img");
    std::fs::copy(fixture("btrfs-subvol.img"), &image).expect("copy the fixture");

    let (rosnap, nested, sub) = {
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open rw"));
        let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
        let rosnap = id_at(&fs, "rosnap").expect("the fixture's rosnap");
        let nested = id_at(&fs, "top/nested").expect("the fixture's top/nested");
        let sub = id_at(&fs, "sub").expect("the fixture's sub");
        let root = fs.root_inode().expect("the top directory").ino;
        let top = fs.lookup(root, b"top").expect("the fixture's top").ino;

        // `sub` holds `inner`, and the kernel refuses it with ENOTEMPTY.
        let generation = fs.superblock().generation;
        assert!(
            matches!(
                fs.delete_subvolume(root, b"sub"),
                Err(Error::UnsupportedFeature(_))
            ),
            "a subvolume holding another was deleted"
        );
        assert_eq!(
            fs.superblock().generation,
            generation,
            "a refusal committed"
        );

        fs.delete_subvolume(root, b"rosnap").expect("delete rosnap");
        assert_eq!(
            fs.superblock().generation,
            generation + 1,
            "one transaction"
        );
        fs.delete_subvolume(top, b"nested")
            .expect("delete top/nested");
        assert_eq!(
            fs.superblock().generation,
            generation + 2,
            "one transaction"
        );
        assert_eq!(id_at(&fs, "rosnap"), None);
        assert_eq!(id_at(&fs, "top/nested"), None);
        assert_eq!(id_at(&fs, "sub"), Some(sub));
        (rosnap, nested, sub)
    };

    assert_btrfs_check_clean(&image, "after the subvolumes were deleted");
    let orphan = |id: u64| format!("ORPHAN ORPHAN_ITEM {id})");
    let root_item = |id: u64| format!("({id} ROOT_ITEM ");
    let dump = dump_tree(&image, "root");
    for id in [rosnap, nested] {
        assert!(
            dump.contains(&orphan(id)),
            "no orphan item for subvolume {id} in the root tree:\n{dump}"
        );
    }

    let image_str = image.to_str().expect("a UTF-8 scratch path").to_string();
    let out = guest_kernel_write_ok(
        &image_str,
        "the deleted subvolumes",
        r#"
cd "$MNT"
btrfs subvolume list . | sed 's/^/list /'
btrfs subvolume sync . >/dev/null && echo synced
btrfs subvolume list -d . | sed 's/^/dead /'
ls -A top | sed 's/^/top /'
"#,
    );
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    let listed: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("list "))
        .filter_map(|l| l.split_whitespace().last())
        .collect();
    assert!(listed.contains(&"sub"), "the kernel's list:\n{out}");
    assert!(listed.contains(&"sub/inner"), "the kernel's list:\n{out}");
    for gone in ["rosnap", "top/nested"] {
        assert!(!listed.contains(&gone), "the kernel lists {gone}:\n{out}");
    }
    assert!(
        lines.contains(&"synced"),
        "the cleaner did not finish:\n{out}"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("dead ")),
        "the kernel still holds a deleted subvolume:\n{out}"
    );
    assert!(
        !lines.contains(&"top nested"),
        "top still shows nested:\n{out}"
    );

    assert_btrfs_check_clean(&image, "after the kernel's cleaner dropped the subvolumes");
    let dump = dump_tree(&image, "root");
    for id in [rosnap, nested] {
        assert!(
            !dump.contains(&orphan(id)) && !dump.contains(&root_item(id)),
            "the cleaner left subvolume {id} in the root tree:\n{dump}"
        );
    }
    assert!(dump.contains(&root_item(sub)), "sub went too:\n{dump}");

    let dev = Arc::new(FileDevice::open(&image).expect("open"));
    let fs = Filesystem::mount(dev).expect("mount");
    assert_eq!(id_at(&fs, "rosnap"), None);
    assert_eq!(id_at(&fs, "top/nested"), None);
    assert_eq!(id_at(&fs, "sub"), Some(sub));
    let _ = std::fs::remove_dir_all(&dir);
}
