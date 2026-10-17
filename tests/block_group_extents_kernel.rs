//! A data block group's extents, and every file item naming each,
//! enumerated through the library and judged by what is not this crate
//! (#396, part of #304).
//!
//! Relocating a block group starts from knowing what is in it: every data
//! extent, and every `EXTENT_DATA` item that has to be pointed somewhere
//! else once the extent moves. The kernel writes a volume holding the
//! shapes that make that hard: a file appended to in several
//! transactions, its reflink, a file partly overwritten in the middle (so
//! two of its items name one extent, the second some way into it), a
//! preallocated file, an inline one, and a subvolume holding a reflink of
//! a top-level file and a file of its own.
//!
//! # The oracles
//!
//! `btrfs inspect-internal dump-tree -t extent` lists the `EXTENT_ITEM`s
//! in each data group: the enumeration must find exactly those, no more
//! and no fewer. Then the kernel's own back-reference walk,
//! `LOGICAL_INO` (`btrfs inspect-internal logical-resolve -P`), is asked
//! about the first byte each enumerated item maps: it answers with every
//! inode, file offset and tree whose items cover that byte, and the
//! enumeration must say exactly the same. Last, once the kernel has
//! snapshotted the volume, the enumeration is refused, because a tree
//! block the snapshot shares names extents from a tree that no
//! reference records.

use fs_btrfs::balance::GroupExtent;
use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::Error;
use fs_btrfs_test_support::{
    dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockRead, FileDevice};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The kernel's fill.
const FILL: &str = "cd \"$MNT\"\n\
     for i in 1 2 3; do\n\
       head -c 65536 /dev/urandom >> appended\n\
       sync\n\
     done\n\
     head -c 100000 /dev/urandom > whole\n\
     head -c 204800 /dev/urandom > overwritten\n\
     sync\n\
     cp --reflink=always appended reflinked\n\
     head -c 8192 /dev/urandom | dd of=overwritten bs=4096 seek=10 conv=notrunc status=none\n\
     fallocate -l 65536 prealloc\n\
     echo small > inline\n\
     btrfs subvolume create sub >/dev/null\n\
     cp --reflink=always whole sub/whole\n\
     head -c 50000 /dev/urandom > sub/own\n\
     sync";

/// The top-level subvolume's tree.
const FS_TREE: u64 = 5;

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
        guest_kernel_write_ok(&image.to_string_lossy(), name, FILL);
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

fn mount(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open(image).expect("open read-only"));
    Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount read-only")
}

/// The groups that hold data and nothing else.
fn data_groups(fs: &Filesystem) -> Vec<BlockGroup> {
    fs.block_groups()
        .unwrap()
        .into_iter()
        .filter(|g| g.holds_data() && !g.holds_metadata())
        .collect()
}

/// Every `EXTENT_ITEM` `dump-tree` lists in `[start, end)`, as `(address,
/// length)`.
fn dumped_extents(image: &Path, start: u64, end: u64) -> BTreeSet<(u64, u64)> {
    dump_tree(image, "extent")
        .lines()
        .filter_map(|line| {
            let key = line.split("key (").nth(1)?;
            let words: Vec<&str> = key.split([' ', ')']).collect();
            (words.get(1) == Some(&"EXTENT_ITEM")).then_some(())?;
            Some((words[0].parse().ok()?, words[2].parse().ok()?))
        })
        .filter(|&(at, _)| (start..end).contains(&at))
        .collect()
}

/// What covers the logical byte `at`, as `(tree, inode, file offset of
/// that byte)`, by the enumeration's account.
fn covering(extents: &[GroupExtent], at: u64) -> BTreeSet<(u64, u64, u64)> {
    extents
        .iter()
        .filter(|e| (e.start..e.start + e.len).contains(&at))
        .flat_map(|e| {
            let into = at - e.start;
            e.items
                .iter()
                .filter(move |r| (r.extent_offset..r.extent_offset + r.len).contains(&into))
                .map(move |r| (r.tree, r.inode, r.file_offset + (into - r.extent_offset)))
        })
        .collect()
}

/// What the kernel's `LOGICAL_INO` says covers each byte of `at`, as
/// `(tree, inode, file offset)`. One guest call for all of them.
fn kernel_resolve(image: &Path, at: &BTreeSet<u64>) -> BTreeMap<u64, BTreeSet<(u64, u64, u64)>> {
    let script: String = at
        .iter()
        .map(|l| {
            format!(
                "echo \"at {l}\"\n\
                 btrfs inspect-internal logical-resolve -P {l} \"$MNT\" || echo \"failed $?\"\n"
            )
        })
        .collect();
    let said = guest_kernel_read_ok(&image.to_string_lossy(), "logical-resolve", &script);
    let mut out: BTreeMap<u64, BTreeSet<(u64, u64, u64)>> = BTreeMap::new();
    let mut current = None;
    for line in said.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["at", l] => {
                let l: u64 = l.parse().unwrap_or_else(|_| panic!("{line:?}"));
                out.entry(l).or_default();
                current = Some(l);
            }
            ["inode", ino, "offset", off, "root", root] => {
                let l = current.unwrap_or_else(|| panic!("{line:?} before any address:\n{said}"));
                let n = |w: &str| w.parse::<u64>().unwrap_or_else(|_| panic!("{line:?}"));
                out.get_mut(&l).unwrap().insert((n(root), n(ino), n(off)));
            }
            _ => panic!("logical-resolve said {line:?}, which this test does not read:\n{said}"),
        }
    }
    assert_eq!(
        out.keys().copied().collect::<BTreeSet<_>>(),
        *at,
        "the kernel did not answer for every address:\n{said}"
    );
    out
}

#[test]
fn a_data_groups_extents_and_their_file_items_are_what_the_kernel_resolves() {
    let scratch = Scratch::new("block-group-extents");
    let image = scratch.image();
    let fs = mount(&image);

    let groups = data_groups(&fs);
    assert!(
        !groups.is_empty(),
        "mkfs.btrfs made no data-only block group"
    );
    let mut all: Vec<GroupExtent> = Vec::new();
    for group in &groups {
        let extents = fs
            .block_group_extents(group.start)
            .unwrap_or_else(|e| panic!("the data group at {}: {e}", group.start));
        let listed: BTreeSet<(u64, u64)> = extents.iter().map(|e| (e.start, e.len)).collect();
        assert_eq!(
            listed.len(),
            extents.len(),
            "an extent was listed twice in the group at {}",
            group.start
        );
        assert!(
            extents.windows(2).all(|w| w[0].start < w[1].start),
            "the extents of the group at {} are not in address order",
            group.start
        );
        assert_eq!(
            listed,
            dumped_extents(&image, group.start, group.end()),
            "the extents enumerated in the group at {} are not the ones dump-tree lists",
            group.start
        );
        all.extend(extents);
    }

    // THE CASES ARE REAL: the fill made what this test is about, so a
    // pass is not a pass over a volume of one extent per file.
    let ino = |name: &str| fs.lookup_path(&format!("/{name}")).unwrap().ino;
    assert!(
        all.iter().any(|e| {
            let trees: BTreeSet<u64> = e.items.iter().map(|r| r.tree).collect();
            trees.contains(&FS_TREE) && trees.iter().any(|&t| t != FS_TREE)
        }),
        "no extent is named from the top level and the subvolume both"
    );
    let overwritten = ino("overwritten");
    assert!(
        all.iter().any(|e| {
            let mine: Vec<_> = e.items.iter().filter(|r| r.inode == overwritten).collect();
            mine.len() == 2 && mine.iter().any(|r| r.extent_offset > 0)
        }),
        "no extent of the partly overwritten file is named twice by it"
    );
    let appended = ino("appended");
    let reflinked = ino("reflinked");
    assert!(
        all.iter().any(|e| {
            let inodes: BTreeSet<u64> = e.items.iter().map(|r| r.inode).collect();
            inodes.contains(&appended) && inodes.contains(&reflinked)
        }),
        "no extent is named by both the appended file and its reflink"
    );
    let prealloc = ino("prealloc");
    assert!(
        all.iter()
            .any(|e| e.items.iter().any(|r| r.inode == prealloc)),
        "the preallocated file's extent was not enumerated"
    );
    let inline = ino("inline");
    assert!(
        all.iter().all(|e| e
            .items
            .iter()
            .all(|r| r.inode != inline || r.tree != FS_TREE)),
        "an inline file was enumerated as naming a data extent"
    );

    // The kernel's back-reference walk, at the first byte each item maps.
    let at: BTreeSet<u64> = all
        .iter()
        .flat_map(|e| e.items.iter().map(move |r| e.start + r.extent_offset))
        .collect();
    let kernel = kernel_resolve(&image, &at);
    for (&l, said) in &kernel {
        assert_eq!(
            &covering(&all, l),
            said,
            "at logical {l}, as (tree, inode, file offset): the enumeration, then the kernel"
        );
    }

    // What is not a data group is refused, by name.
    let metadata = fs
        .block_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.holds_metadata())
        .expect("a metadata group");
    match fs.block_group_extents(metadata.start) {
        Err(Error::UnsupportedFeature(why)) => assert!(why.contains("data"), "{why}"),
        other => panic!("a metadata group was enumerated: {other:?}"),
    }
    assert!(
        fs.block_group_extents(1).is_err(),
        "an address no group starts at was enumerated"
    );
    drop(fs);

    // A snapshot shares the tree blocks holding the file items, so the
    // extents are named from a tree no reference records: refused.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel snapshots the volume",
        "btrfs subvolume snapshot \"$MNT\" \"$MNT/snap\" >/dev/null\nsync",
    );
    let fs = mount(&image);
    for group in data_groups(&fs) {
        match fs.block_group_extents(group.start) {
            Err(Error::UnsupportedFeature(why)) => assert!(why.contains("snapshot"), "{why}"),
            other => panic!("a snapshotted volume's data group was enumerated: {other:?}"),
        }
    }
}
