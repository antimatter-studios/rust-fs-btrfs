//! Writes on a block-group-tree volume, judged by the guest kernel and
//! btrfs-progs (#451). The existing fixture was made with
//! `mkfs.btrfs -O block-group-tree`; a second fresh volume checks chunk
//! creation and removal, which must file group items in the same tree.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, fixture, guest_kernel_read_ok, guest_kernel_write_ok,
    oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        Self(dir)
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
    let dev = Arc::new(FileDevice::open_rw(image).expect("open writable image"));
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount block group tree rw");
    assert!(fs.superblock().has_block_group_tree());
    fs
}

fn check_groups(image: &Path) {
    assert_btrfs_check_clean(image, "block group tree write");
    let extent = dump_tree(image, "2");
    assert!(
        !extent.contains("BLOCK_GROUP_ITEM"),
        "block group items must not enter the extent tree:\n{extent}"
    );
    let groups = dump_tree(image, "11");
    assert!(groups.contains("BLOCK_GROUP_ITEM"), "tree 11 is empty");
}

#[test]
fn create_overwrite_and_unlink_on_a_block_group_tree_are_clean_and_read_by_the_kernel() {
    let scratch = Scratch::new("block-group-tree-write");
    let image = scratch.image();
    std::fs::copy(fixture("features/btrfs-bgt.img"), &image).expect("copy the kernel fixture");
    let img = image.to_string_lossy();
    guest_kernel_write_ok(
        &img,
        "prepare block group tree files",
        "head -c 65536 /dev/zero | tr '\\0' O > \"$MNT/bgt-overwrite\"\n\
         head -c 32768 /dev/zero | tr '\\0' D > \"$MNT/bgt-unlink\"\n\
         sync",
    );

    {
        let mut fs = mount_rw(&image);
        let old_root = fs.tree_root_public(11).expect("block group root");
        let parent = fs.lookup_path("/").expect("root directory").ino;
        let new = fs
            .create(parent, b"bgt-created", 0o644, 0, 0)
            .expect("create");
        assert_eq!(
            fs.write(new, 0, &[b'N'; 8192]).expect("write new file"),
            8192
        );
        assert_ne!(
            fs.tree_root_public(11).unwrap(),
            old_root,
            "group tree is copied on write"
        );
    }
    check_groups(&image);
    {
        let mut fs = mount_rw(&image);
        let ino = fs.lookup_path("/bgt-overwrite").expect("existing file").ino;
        assert_eq!(fs.write(ino, 0, &[b'W'; 65536]).expect("overwrite"), 65536);
    }
    check_groups(&image);
    {
        let mut fs = mount_rw(&image);
        let parent = fs.lookup_path("/").expect("root directory").ino;
        fs.unlink(parent, b"bgt-unlink").expect("unlink");
    }
    check_groups(&image);

    // Compare the exact byte streams, including lengths, inside the guest.
    guest_kernel_read_ok(
        &img,
        "read block group tree writes",
        "head -c 8192 /dev/zero | tr '\\0' N > /tmp/bgt-want-new\n\
         head -c 65536 /dev/zero | tr '\\0' W > /tmp/bgt-want-overwrite\n\
         cmp /tmp/bgt-want-new \"$MNT/bgt-created\"\n\
         cmp /tmp/bgt-want-overwrite \"$MNT/bgt-overwrite\"\n\
         test ! -e \"$MNT/bgt-unlink\"",
    );
    guest_kernel_write_ok(
        &img,
        "scrub block group tree writes",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

#[test]
fn relocating_an_empty_group_keeps_its_items_in_the_block_group_tree() {
    let scratch = Scratch::new("block-group-tree-relocate");
    let image = scratch.image();
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .unwrap();
    // The guest's pinned static mkfs knows this feature; Debian's mkfs
    // does not. This is the same tool the bgt fixture builder uses.
    let made = oracle("/opt/btrfs-progs-static/mkfs.btrfs")
        .args([
            "-q",
            "-f",
            "-s",
            "4096",
            "-n",
            "16384",
            "-O",
            "block-group-tree",
        ])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let (old, new) = {
        let mut fs = mount_rw(&image);
        let group = fs
            .block_groups()
            .unwrap()
            .into_iter()
            .find(|g| g.holds_data() && !g.holds_metadata())
            .expect("data group");
        assert_eq!(group.used, 0);
        let new = fs
            .relocate_block_group(group.start, None)
            .expect("relocate empty group");
        assert_ne!(new, group.start);
        (group.start, new)
    };
    check_groups(&image);
    let groups = dump_tree(&image, "11");
    assert!(!groups.contains(&format!("key ({old} BLOCK_GROUP_ITEM ")));
    assert!(groups.contains(&format!("key ({new} BLOCK_GROUP_ITEM ")));
    let img = image.to_string_lossy();
    guest_kernel_write_ok(
        &img,
        "kernel writes the relocated block group",
        "printf 'block group tree relocation\\n' > \"$MNT/relocated\"\n\
         sync",
    );
    let seen = guest_kernel_read_ok(&img, "read relocated group", "cat \"$MNT/relocated\"");
    assert_eq!(seen, "block group tree relocation\n");
    check_groups(&image);
}
