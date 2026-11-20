//! Salvage on truncated images: what lies past the end is reported
//! unreadable, and everything before it is still recovered.
//!
//! `btrfs-comp-zstd.img` keeps its data chunk at 13 MiB and its metadata as
//! DUP, with copy 0 from 37 MiB and copy 1 past 64 MiB. Cut at 64 MiB, the
//! second superblock copy and every second metadata copy are gone, and the
//! first copies still give every file. Cut at the filesystem tree root's
//! first copy, that root is lost in both copies, and salvage names it. `btrfs restore -i`, in the harness VM, is the
//! oracle: salvage must be right wherever restore is.

use fs_btrfs::salvage::{
    best_copy, build_chunk_map, collect_inodes, copy_file, read_root_items, root_tree_candidates,
    superblock_copies, BlockFault, CopyVerdict, CsumTree, InodeScan, Layout,
};
use fs_btrfs_test_support::{copy_sparse, fixture, oracle, sha256_file, sha256_hex, temp_path};
use fs_core::FileDevice;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const MIB: u64 = 1 << 20;

type Listing = BTreeMap<String, Option<String>>;

/// The filesystem tree through the newest root tree that names it.
fn gather(device: &FileDevice) -> Option<(Layout, InodeScan, Option<CsumTree>)> {
    let copies = superblock_copies(device);
    let sb = best_copy(&copies)?.superblock.clone()?;
    let layout = Layout::new(&sb, build_chunk_map(device, &sb).map);
    for candidate in root_tree_candidates(&copies) {
        let (roots, _) = read_root_items(device, &layout, candidate.tree_root);
        if let Some(fs) = roots.iter().find(|r| r.objectid == 5 && r.key_offset == 0) {
            let csums = roots
                .iter()
                .find(|r| r.objectid == 7)
                .map(|r| CsumTree::new(r.pointer()));
            let scan = collect_inodes(device, &layout, fs.pointer());
            return Some((layout, scan, csums));
        }
    }
    None
}

/// Directories and regular files reachable by name from the root
/// directory, each file hashed from its extents.
fn salvaged(device: &FileDevice) -> (Listing, Option<InodeScan>) {
    let Some((layout, scan, csums)) = gather(device) else {
        return (Listing::new(), None);
    };
    let mut children: BTreeMap<u64, Vec<(Vec<u8>, u64)>> = BTreeMap::new();
    for record in scan.inodes.values() {
        for claim in record.names.iter().filter(|c| c.from_directory) {
            children
                .entry(claim.parent)
                .or_default()
                .push((claim.name.clone(), record.ino));
        }
    }
    let mut listing = Listing::new();
    let mut stack = vec![(256u64, String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for (name, ino) in children.get(&dir).cloned().unwrap_or_default() {
            let name = String::from_utf8_lossy(&name).into_owned();
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let Some(record) = scan.inodes.get(&ino) else {
                continue;
            };
            let Some(inode) = &record.inode else { continue };
            match inode.mode & S_IFMT {
                S_IFDIR => {
                    listing.insert(path.clone(), None);
                    stack.push((ino, path));
                }
                S_IFREG => {
                    let mut bytes = Vec::new();
                    copy_file(device, &layout, csums.as_ref(), record, &mut bytes).unwrap();
                    listing.insert(path, Some(sha256_hex(&bytes)));
                }
                _ => {}
            }
        }
    }
    (listing, Some(scan))
}

/// What `btrfs restore -i` writes, read back; empty when it gives up.
fn restored(image: &Path, out: &Path) -> Listing {
    std::fs::create_dir_all(out).unwrap();
    oracle("btrfs")
        .args(["restore", "-i"])
        .arg(image)
        .arg(out)
        .output();
    let mut listing = Listing::new();
    let mut stack = vec![out.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path
                .strip_prefix(out)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                listing.insert(name, None);
                stack.push(path);
            } else if path.is_file() {
                listing.insert(name, Some(sha256_file(&path).unwrap()));
            }
        }
    }
    listing
}

fn truncated(tag: &str, length: u64) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(temp_path!("salvage-truncated-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("comp-zstd.img");
    copy_sparse(fixture("btrfs-comp-zstd.img"), &image).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap()
        .set_len(length)
        .unwrap();
    (image, dir)
}

fn healthy() -> Listing {
    let (listing, _) = salvaged(&FileDevice::open(fixture("btrfs-comp-zstd.img")).unwrap());
    assert!(listing.values().filter(|v| v.is_some()).count() >= 3);
    listing
}

/// Cut at 64 MiB: the second superblock copy is absent, the first copy of
/// every tree block still verifies, and every file is recovered as it was,
/// as restore also recovers it.
#[test]
fn cut_at_64_mib_every_file_is_still_recovered() {
    let truth = healthy();
    let (image, dir) = truncated("64m", 64 * MIB);
    let device = FileDevice::open(&image).unwrap();
    let copies = superblock_copies(&device);
    assert_eq!(copies[0].verdict, CopyVerdict::Valid);
    assert_eq!(copies[1].verdict, CopyVerdict::Absent);
    assert_eq!(copies[2].verdict, CopyVerdict::Absent);

    let (ours, scan) = salvaged(&device);
    assert!(
        scan.unwrap().walk.damaged.is_empty(),
        "copy 0 of every block verifies"
    );
    assert_eq!(ours, truth);
    assert_eq!(restored(&image, &dir.join("restore")), truth);
}

/// Cut at 64 MiB with a leaf's first copy broken as well: its second copy
/// lay past the end, so the leaf is reported with a checksum fault on copy
/// 0 and an unreadable copy 1, and the walk still hands over its items.
#[test]
fn cut_at_64_mib_a_broken_first_copy_has_no_second_to_fall_back_on() {
    let (image, _) = truncated("64m-leaf", 64 * MIB);
    let device = FileDevice::open(&image).unwrap();
    let (layout, _, _) = gather(&device).unwrap();
    let copies = superblock_copies(&device);
    let (roots, _) = read_root_items(&device, &layout, root_tree_candidates(&copies)[0].tree_root);
    let fs = roots.iter().find(|r| r.objectid == 5).unwrap().pointer();
    let at =
        layout.chunks.map(fs.bytenr).unwrap().physical + u64::from(layout.geometry.nodesize) - 1;
    assert!(layout.chunks.map_mirror(fs.bytenr, 1).unwrap().physical >= 64 * MIB);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image)
        .unwrap();
    let mut byte = [0u8];
    file.read_exact_at(&mut byte, at).unwrap();
    file.write_all_at(&[byte[0] ^ 0xff], at).unwrap();

    let device = FileDevice::open(&image).unwrap();
    let (_, scan, _) = gather(&device).unwrap();
    let damage = scan
        .walk
        .damaged
        .iter()
        .find(|d| d.logical == fs.bytenr)
        .expect("the filesystem tree's root is reported");
    assert_eq!(damage.attempts.len(), 2);
    assert_eq!(damage.attempts[0].faults, vec![BlockFault::Checksum]);
    assert!(matches!(
        damage.attempts[1].faults[..],
        [BlockFault::Unreadable(_)]
    ));
    assert!(damage.decoded);
    assert!(
        !scan.inodes.is_empty(),
        "the root's items are still handed over"
    );
}

/// Cut at the current filesystem tree root's first copy, inside the first
/// metadata copies: that root, and its second copy past 64 MiB, are lost.
/// On this volume every file's items were in that one block and the older
/// root trees hold an empty filesystem tree, so neither tool recovers a
/// file. Salvage names the block unreadable in both copies, still maps the
/// system chunks, does not fail, and is right wherever restore is.
#[test]
fn cut_through_the_filesystem_root_the_loss_is_named() {
    let truth = healthy();
    let healthy_device = FileDevice::open(fixture("btrfs-comp-zstd.img")).unwrap();
    let copies = superblock_copies(&healthy_device);
    let sb = copies[0].superblock.clone().unwrap();
    let layout = Layout::new(&sb, build_chunk_map(&healthy_device, &sb).map);
    let (roots, _) = read_root_items(
        &healthy_device,
        &layout,
        root_tree_candidates(&copies)[0].tree_root,
    );
    let fs = roots.iter().find(|r| r.objectid == 5).unwrap().pointer();
    let cut = layout.chunks.map(fs.bytenr).unwrap().physical;
    assert!(cut > 22 * MIB && cut < 64 * MIB, "cut at {cut}");

    let (image, dir) = truncated("fs-root", cut);
    let device = FileDevice::open(&image).unwrap();
    let lost = fs_btrfs::salvage::read_block(
        &device,
        &layout,
        fs.bytenr,
        fs_btrfs::salvage::Expect::default(),
    );
    assert!(
        lost.attempts
            .iter()
            .all(|a| matches!(a.faults[..], [BlockFault::Unreadable(_)])),
        "{:?}",
        lost.attempts
    );

    let (ours, _) = salvaged(&device);
    let theirs = restored(&image, &dir.join("restore"));
    assert_ne!(theirs, truth, "the cut cost restore something");
    for (path, hash) in &truth {
        let ours_ok = ours.get(path) == Some(hash);
        let theirs_ok = theirs.get(path) == Some(hash);
        assert!(
            ours_ok || !theirs_ok,
            "{path}: restore recovered it and salvage did not"
        );
    }
    let copies = superblock_copies(&device);
    assert_eq!(copies[0].verdict, CopyVerdict::Valid);
    let map = build_chunk_map(&device, &copies[0].superblock.clone().unwrap());
    assert!(map.from_system_array > 0, "the system chunks still map");
}
