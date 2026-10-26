//! A full root node splits, and its tree grows a level (#513). With
//! 4 KiB nodes a level-1 node holds 121 key pointers, so a directory of
//! some hundreds of names fills the fs tree's root node; the next leaf
//! split below it needs a pointer the node has no room for. Names go in
//! through the C ABI until the root is a level-2 node, and then
//! btrfs-progs must find the volume clean, its dump of tree 5 must show
//! that root over two level-1 nodes, and the kernel must list every name.

use fs_btrfs::capi::*;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Far more names than fill one level-1 node's pointers; the run stops
/// at the first name after the root splits.
const LIMIT: usize = 4000;
/// How many names go in between looks at the tree's height.
const BATCH: usize = 20;
const PREFIX: &str = "a-file-with-a-longish-name-";

/// A scratch image, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// The level of tree 5's root, read from the image as it stands.
fn fs_tree_level(image: &Path) -> u8 {
    let dev = Arc::new(FileDevice::open(image).expect("open the image"));
    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount");
    let root = fs.tree_root_public(5).expect("tree 5's root");
    fs.read_tree_block(root)
        .expect("tree 5's root block")
        .header
        .level
}

#[test]
fn a_full_root_node_splits_and_the_tree_grows_a_level() {
    let dir = Scratch(PathBuf::from(temp_path!("node-split-kernel")));
    std::fs::create_dir_all(&dir.0).unwrap();
    let image = dir.0.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .unwrap();
    {
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open the image"));
        let opts = fs_btrfs::mkfs::Options {
            nodesize: 4096,
            ..fs_btrfs::mkfs::Options::default()
        };
        fs_btrfs::mkfs::format(dev.as_ref() as &dyn BlockDevice, &opts).expect("mkfs");
    }

    let device = c(image.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount_rw(device.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    let mut made = 0usize;
    while made < LIMIT {
        for _ in 0..BATCH {
            let name = format!("/{PREFIX}{made:05}");
            let ino = unsafe { fs_btrfs_create(fs, c(&name).as_ptr(), 0o644) };
            assert_ne!(ino, 0, "create {} ({name}): {}", made + 1, last_error());
            made += 1;
        }
        if fs_tree_level(&image) >= 2 {
            break;
        }
    }
    unsafe { fs_btrfs_umount(fs) };
    assert_eq!(
        fs_tree_level(&image),
        2,
        "{made} names and tree 5's root is not a level-2 node"
    );

    assert_btrfs_check_clean(&image, "a root node split under a directory");

    let dump = dump_tree(&image, "5");
    let root = dump
        .lines()
        .find(|l| l.starts_with("node "))
        .expect("dump-tree prints tree 5's root");
    assert!(
        root.contains(" level 2 "),
        "tree 5's root is not at level 2: {root}"
    );
    let level1 = dump
        .lines()
        .filter(|l| l.starts_with("node ") && l.contains(" level 1 "))
        .count();
    assert_eq!(
        level1, 2,
        "a split root has two level-1 nodes below it, dump-tree shows {level1}"
    );

    let listed = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "node split",
        &format!("ls \"$MNT\" | grep -c '^{PREFIX}'"),
    );
    assert_eq!(
        listed.trim(),
        made.to_string(),
        "the kernel lists {listed} of the {made} names"
    );
}

/// The root's level and number of child pointers, without a guest round trip.
fn fs_tree_shape(image: &Path) -> (u8, usize) {
    let dev = Arc::new(FileDevice::open(image).expect("open the image"));
    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount");
    let root = fs.tree_root_public(5).expect("tree 5's root");
    let block = fs.read_tree_block(root).expect("tree 5's root block");
    (
        block.header.level,
        block.body.key_ptrs().map_or(0, <[_]>::len),
    )
}

#[test]
fn a_full_non_root_node_splits_into_a_parent_with_room() {
    let dir = Scratch(PathBuf::from(temp_path!("non-root-node-split-kernel")));
    std::fs::create_dir_all(&dir.0).unwrap();
    let image = dir.0.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .unwrap();
    {
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open the image"));
        let opts = fs_btrfs::mkfs::Options {
            nodesize: 4096,
            ..fs_btrfs::mkfs::Options::default()
        };
        fs_btrfs::mkfs::format(dev.as_ref() as &dyn BlockDevice, &opts).expect("mkfs");
    }

    let device = c(image.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount_rw(device.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    let mut made = 0usize;
    let mut saw_two_children = false;
    while made < LIMIT {
        for _ in 0..BATCH {
            let name = format!("/{PREFIX}{made:05}");
            let ino = unsafe { fs_btrfs_create(fs, c(&name).as_ptr(), 0o644) };
            assert_ne!(ino, 0, "create {} ({name}): {}", made + 1, last_error());
            made += 1;
        }
        let (level, children) = fs_tree_shape(&image);
        if level == 2 {
            if children == 2 {
                saw_two_children = true;
            } else if children >= 3 {
                break;
            }
        }
    }
    unsafe { fs_btrfs_umount(fs) };
    assert!(
        saw_two_children,
        "the root first grows over two level-1 nodes"
    );
    assert_eq!(
        fs_tree_shape(&image),
        (2, 3),
        "{made} names must split a non-root node without growing the root"
    );

    assert_btrfs_check_clean(&image, "a non-root node split under a directory");
    let dump = dump_tree(&image, "5");
    let root = dump
        .lines()
        .find(|l| l.starts_with("node "))
        .expect("dump-tree prints tree 5's root");
    assert!(
        root.contains(" level 2 "),
        "the root stays at level 2: {root}"
    );
    assert_eq!(
        dump.lines()
            .filter(|l| l.starts_with("node ") && l.contains(" level 1 "))
            .count(),
        3,
        "dump-tree must find one more level-1 node after the non-root split"
    );

    let listed = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "non-root node split",
        "LC_ALL=C ls -1 \"$MNT\" | LC_ALL=C sort",
    );
    let expected: Vec<String> = (0..made).map(|n| format!("{PREFIX}{n:05}")).collect();
    assert_eq!(listed.lines().collect::<Vec<_>>(), expected);
}

/// Large attributes put pressure on the inode leaves without requiring
/// hundreds of thousands of names. A level-2 root can grow to level 3
/// only when a level-1 split below it also overflows that root (#515).
#[test]
fn full_ancestors_split_together_up_to_a_new_root() {
    let dir = Scratch(PathBuf::from(temp_path!("cascade-node-split-kernel")));
    std::fs::create_dir_all(&dir.0).unwrap();
    let image = dir.0.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(2 << 30))
        .unwrap();
    // The kernel's bulk fill fragments free space enough to select v2
    // bitmaps, whose rewriting is unsupported. The v1 cache is supported
    // and keeps this case focused on cascading tree splits.
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-n", "4096", "-R", "^free-space-tree"])
        .arg(&image)
        .output();
    assert!(made.status.success(), "mkfs: {:?}", made);

    // The kernel builds the bulk of the fixture in shared transactions.
    // Stop at a nearly full level-2 root, then let the driver fill the
    // remaining slots and split it. Building every padded inode through
    // two driver commits took over three hours before the check (#686).
    let filled = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "a nearly full level-2 root before the cascading split",
        &format!(
            r#"mount -o remount,space_cache=v1 "$MNT"
python3 - "$MNT" <<'PY'
import os, subprocess, sys
root = sys.argv[1]
image = subprocess.check_output(
    ['findmnt', '-n', '-o', 'SOURCE', '--target', root], text=True).strip().split('[')[0]
def shape():
    subprocess.run(['btrfs', 'filesystem', 'sync', root], check=True,
                   stdout=subprocess.DEVNULL)
    roots = subprocess.check_output(
        ['btrfs', 'inspect-internal', 'dump-tree', '-r', '-t', '5', image], text=True)
    line = next(line for line in roots.splitlines() if line.startswith('fs tree key '))
    address, _, level = line.rsplit(') ', 1)[1].split()
    block = subprocess.check_output(
        ['btrfs', 'inspect-internal', 'dump-tree', '-b', address, image], text=True)
    header = next(line for line in block.splitlines()
                  if line.startswith(('node ', 'leaf '))).split()
    return int(level), int(header[header.index('items') + 1])
made = 0
level, items = shape()
while made < 30000:
    batch = 1 if level == 2 and items >= 110 else 64
    for _ in range(batch):
        path = os.path.join(root, '{PREFIX}' + format(made, '05d'))
        with open(path, 'xb'):
            pass
        made += 1
        if batch == 1:
            level, items = shape()
            if level == 2 and items >= 117:
                break
        os.setxattr(path, 'user.padding', b'a' * 3000)
    level, items = shape()
    assert level < 3, ('the kernel passed the nearly full root', made, level, items)
    # Linux reserves three of 121 slots, and a create inserts several
    # items. Stop one slot earlier, before a create can grow the root.
    if level == 2 and items >= 117:
        print(made)
        break
else:
    raise AssertionError(('the kernel never filled the root', made, level, items))
PY
"#
        ),
    );
    let mut made: usize = filled.trim().parse().expect("the kernel's file count");
    let (level, children) = fs_tree_shape(&image);
    assert_eq!(level, 2, "the cascade starts below a level-2 root");
    assert!(
        (117..=121).contains(&children),
        "the root must start nearly full"
    );
    let limit = made + 768;
    let fs = unsafe { fs_btrfs_mount_rw(c(image.to_str().unwrap()).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    let padding = vec![0x61u8; 3000];
    let mut saw_level_two = true;
    while made < limit {
        for _ in 0..BATCH {
            let name = c(&format!("/{PREFIX}{made:05}"));
            let ino = unsafe { fs_btrfs_create(fs, name.as_ptr(), 0o644) };
            assert_ne!(ino, 0, "create {}: {}", made + 1, last_error());
            let rc = unsafe {
                fs_btrfs_setxattr(
                    fs,
                    name.as_ptr(),
                    c("user.padding").as_ptr(),
                    padding.as_ptr().cast(),
                    padding.len(),
                )
            };
            assert_eq!(rc, 0, "pad {}: {}", made + 1, last_error());
            made += 1;
        }
        match fs_tree_level(&image) {
            2 => saw_level_two = true,
            3 => break,
            _ => {}
        }
    }
    unsafe { fs_btrfs_umount(fs) };
    assert!(saw_level_two, "the cascade starts below a level-2 root");
    assert_eq!(
        fs_tree_level(&image),
        3,
        "{made} padded names must cascade through both node levels"
    );
    assert_btrfs_check_clean(&image, "a split cascades through two full ancestors");
    let dump = dump_tree(&image, "5");
    for (level, expected) in [(3, 1), (2, 2)] {
        assert_eq!(
            dump.lines()
                .filter(|l| l.starts_with("node ") && l.contains(&format!(" level {level} ")))
                .count(),
            expected,
            "dump-tree must find {expected} level-{level} nodes after the cascade"
        );
    }
    assert!(
        dump.lines()
            .filter(|l| l.starts_with("node ") && l.contains(" level 1 "))
            .count()
            >= 122,
        "the former root's full pointer array must have overflowed"
    );
    let listed = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "cascading node splits",
        "LC_ALL=C ls -1 \"$MNT\" | LC_ALL=C sort",
    );
    let expected: Vec<String> = (0..made).map(|n| format!("{PREFIX}{n:05}")).collect();
    assert_eq!(listed.lines().collect::<Vec<_>>(), expected);
}
