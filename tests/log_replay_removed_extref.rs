//! An inode logged only to say it exists keeps its committed extended
//! references. One that the log removes, by speaking for its index in the
//! directory and not holding it, is refused before anything is written
//! (#476, #722).
//!
//! The log here is CONSTRUCTED, not made by a kernel. The native crash image
//! of the extended-reference hard link logs `d` and its new name for `f`, and
//! says nothing about `/`, where `f`'s committed extended references are. Each
//! test adds to a copy of that log the committed inode of `/` and one index
//! range of it, rebuilds the log's first leaf and gives it a fresh checksum.

// The read-only opener deprecated in 0.12.0 stays in use here for its
// deprecation release (`layers::BtrfsVolume` replaces it).
#![allow(deprecated)]

use fs_btrfs::btree::{Body, HEADER_SIZE};
use fs_btrfs::chunk::DiskKey;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::inode::INODE_ITEM_KEY;
use fs_btrfs::log_tree::DIR_LOG_INDEX_KEY;
use fs_btrfs::tree_write::{build_leaf, stamp_checksum, BlockIdentity, LeafItem};
use fs_btrfs::Error;
use fs_btrfs_test_support::{copy_sparse, fixture, sha256_file, temp_path};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FIXTURE: &str = "dirtylog/btrfs-dirty-log-extrefexists.img";
/// `BTRFS_INODE_EXTREF_KEY`.
const INODE_EXTREF_KEY: u8 = 13;
/// `BTRFS_ROOT_ITEM_KEY`, and where a root item holds its tree's address.
const ROOT_ITEM_KEY: u8 = 132;
const ROOT_ITEM_BYTENR: usize = 176;
/// The top-level directory.
const ROOT_DIR: u64 = 256;
/// Where an inode item holds its size.
const INODE_SIZE: std::ops::Range<usize> = 16..24;

/// A scratch copy of the fixture, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let scratch = Scratch(PathBuf::from(temp_path!("removed-extref-{tag}.img")));
        copy_sparse(fixture(FIXTURE), &scratch.0).expect("copy the fixture to scratch");
        scratch
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn committed_view(image: &Path) -> Filesystem {
    Filesystem::mount_ignoring_log(
        Arc::new(FileDevice::open(image).expect("open the copy")) as Arc<dyn BlockRead>
    )
    .expect("read the copy without replay")
}

/// Every item of the tree rooted at `at`, in key order.
fn items_under(fs: &Filesystem, at: u64, out: &mut Vec<(DiskKey, Vec<u8>)>) {
    let block = fs.read_tree_block(at).expect("read a tree block");
    match &block.body {
        Body::Node(children) => {
            for child in children {
                items_under(fs, child.blockptr, out);
            }
        }
        Body::Leaf(items) => {
            for item in items {
                let data = block.item_data(item).expect("the item's data");
                out.push((item.key, data.to_vec()));
            }
        }
    }
}

/// What the constructed log needs from the committed top-level tree.
struct Committed {
    /// The inode item of `/`.
    root_dir: Vec<u8>,
    /// One name an extended reference gives in `/`: the file, the name's
    /// index and its length.
    ino: u64,
    index: u64,
    name_len: u64,
}

fn committed(fs: &Filesystem) -> Committed {
    let root = fs
        .root_tree_items()
        .expect("the root tree")
        .into_iter()
        .find(|&(objectid, kind, _, _)| (objectid, kind) == (5, ROOT_ITEM_KEY))
        .expect("the top-level tree's root item")
        .3;
    let at = u64::from_le_bytes(
        root[ROOT_ITEM_BYTENR..ROOT_ITEM_BYTENR + 8]
            .try_into()
            .unwrap(),
    );
    let mut items = Vec::new();
    items_under(fs, at, &mut items);
    let root_dir = items
        .iter()
        .find(|(key, _)| (key.objectid, key.key_type) == (ROOT_DIR, INODE_ITEM_KEY))
        .expect("the inode item of /")
        .1
        .clone();
    let f = fs
        .lookup_path("/f")
        .expect("the committed trees hold /f")
        .ino;
    // A record is parent, index, name length, name.
    let (_, record) = items
        .iter()
        .find(|(key, _)| (key.objectid, key.key_type) == (f, INODE_EXTREF_KEY))
        .expect("f is committed with an extended reference");
    let word = |at: usize| u64::from_le_bytes(record[at..at + 8].try_into().unwrap());
    assert_eq!(word(0), ROOT_DIR, "the extended reference is in /");
    Committed {
        root_dir,
        ino: f,
        index: word(8),
        name_len: u64::from(u16::from_le_bytes(record[16..18].try_into().unwrap())),
    }
}

/// Write a tree block to every copy of it.
fn write_block(fs: &Filesystem, image: &Path, logical: u64, bytes: &[u8]) {
    let file = std::fs::OpenOptions::new().write(true).open(image).unwrap();
    for mirror in 0..fs.chunk_map().mirrors_at(logical).unwrap() {
        let at = fs.chunk_map().map_mirror(logical, mirror).unwrap();
        file.write_all_at(bytes, at.physical).unwrap();
    }
}

/// Add to the top level's log, in place, the inode of `/` and an index range
/// of it holding `index` alone, with `/`'s size less `removed` bytes of
/// names (each name counts twice). `/` sorts before everything the log holds,
/// so both go at the front of its first leaf, and every node above that leaf
/// is given its new first key.
fn log_the_root_directory(image: &Path, root_dir: &[u8], index: u64, removed: u64) {
    let fs = committed_view(image);
    let log = fs.log().unwrap().expect("the copy holds a log");
    let top = log
        .trees
        .iter()
        .find(|t| t.subvolume == 5)
        .expect("the copy holds a log of the top-level subvolume");
    assert!(
        top.items.iter().all(|i| i.key.objectid > ROOT_DIR),
        "the native log says nothing about /"
    );
    // The path from the log's root down to its first leaf.
    let mut nodes = Vec::new();
    let mut block = fs.read_tree_block(top.root).unwrap();
    while let Body::Node(children) = &block.body {
        let child = children[0].blockptr;
        nodes.push(block);
        block = fs.read_tree_block(child).unwrap();
    }
    let mut items = Vec::new();
    items_under(&fs, block.header.bytenr, &mut items);

    let mut inode = root_dir.to_vec();
    let size = u64::from_le_bytes(inode[INODE_SIZE].try_into().unwrap());
    inode[INODE_SIZE].copy_from_slice(&(size - 2 * removed).to_le_bytes());
    let key = |key_type: u8, offset: u64| DiskKey {
        objectid: ROOT_DIR,
        key_type,
        offset,
    };
    items.push((key(INODE_ITEM_KEY, 0), inode));
    items.push((key(DIR_LOG_INDEX_KEY, index), index.to_le_bytes().to_vec()));
    items.sort_by_key(|(key, _)| (key.objectid, key.key_type, key.offset));

    let leaf: Vec<LeafItem> = items
        .iter()
        .map(|(key, data)| LeafItem { key: *key, data })
        .collect();
    let header = &block.header;
    let bytes = build_leaf(
        fs.superblock(),
        BlockIdentity {
            bytenr: header.bytenr,
            owner: header.owner,
            generation: header.generation,
            level: 0,
            flags: header.flags,
            chunk_tree_uuid: header.chunk_tree_uuid,
        },
        &leaf,
    )
    .expect("rebuild the log's first leaf");
    write_block(&fs, image, header.bytenr, &bytes);

    // A node's first key pointer starts with the key: objectid, type, offset.
    let first = items[0].0;
    for node in &nodes {
        let mut bytes = node.bytes().to_vec();
        bytes[HEADER_SIZE..HEADER_SIZE + 8].copy_from_slice(&first.objectid.to_le_bytes());
        bytes[HEADER_SIZE + 8] = first.key_type;
        bytes[HEADER_SIZE + 9..HEADER_SIZE + 17].copy_from_slice(&first.offset.to_le_bytes());
        stamp_checksum(&mut bytes, fs.superblock());
        write_block(&fs, image, node.header.bytenr, &bytes);
    }
}

fn mount_rw(image: &Path) -> fs_btrfs::Result<Filesystem> {
    Filesystem::mount_rw(
        Arc::new(FileDevice::open_rw(image).expect("open the copy")) as Arc<dyn BlockDevice>
    )
}

/// The range holds the index of a name only an extended reference gives, and
/// the log holds no entry there: the log removes that name.
#[test]
fn an_exists_only_inode_losing_an_extended_reference_name_is_refused_without_writing() {
    let scratch = Scratch::new("removed");
    let seen = committed(&committed_view(&scratch.0));
    log_the_root_directory(&scratch.0, &seen.root_dir, seen.index, seen.name_len);
    {
        let fs = committed_view(&scratch.0);
        let log = fs.log().unwrap().expect("the rebuilt log reads");
        let top = log.trees.iter().find(|t| t.subvolume == 5).unwrap();
        let logged = top.inode(seen.ino).unwrap().expect("f is still logged");
        assert_eq!(logged.generation, 0, "f is logged only as existing");
    }
    let before = sha256_file(&scratch.0).unwrap();
    let result = mount_rw(&scratch.0);
    assert!(
        matches!(result, Err(Error::DirtyLog)),
        "removing a committed extended-reference name must be refused with DirtyLog: {:?}",
        result.as_ref().err()
    );
    drop(result);
    assert_eq!(
        before,
        sha256_file(&scratch.0).unwrap(),
        "the refusal wrote to the image"
    );
}

/// The control for the construction: the same additions with a range that
/// holds no committed name remove nothing, and the log replays.
#[test]
fn the_same_log_with_a_range_that_removes_no_name_replays() {
    let scratch = Scratch::new("kept");
    let seen = committed(&committed_view(&scratch.0));
    log_the_root_directory(&scratch.0, &seen.root_dir, u64::MAX - 1, 0);
    let fs = mount_rw(&scratch.0).expect("a log removing no name replays");
    assert_eq!(fs.superblock().log_root, 0, "the log was replayed");
    let f = fs.lookup_path("/f").expect("f after replay");
    assert_eq!((f.ino, f.nlink), (seen.ino, 66), "f keeps every name");
}
