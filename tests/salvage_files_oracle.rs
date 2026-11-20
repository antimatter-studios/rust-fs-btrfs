//! Salvage recovers files as `btrfs restore` recovers them.
//!
//! `btrfs restore` is btrfs-progs' own salvage tool: it reads a volume
//! without mounting it and writes out every file it can reach. Run in the
//! harness VM into this repository's scratch directory, its output is the
//! oracle: the files salvage gathers from the filesystem tree, named
//! through their directory entries and read through their extents, have to
//! be the same paths with the same bytes, compressed or not, and on a
//! volume with a broken leaf salvage has to recover at least what restore
//! does, byte for byte.

use fs_btrfs::salvage::{
    best_copy, build_chunk_map, collect_inodes, copy_file, read_root_items, root_tree_candidates,
    superblock_copies, walk_tree, InodeScan, Layout, RootPointer,
};
use fs_btrfs_test_support::{copy_sparse, fixture, oracle, sha256_file, sha256_hex, temp_path};
use fs_core::FileDevice;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::ops::ControlFlow;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const ROOT_DIR: u64 = 256;

/// Path -> `None` for a directory, the SHA-256 of the bytes for a file.
type Listing = BTreeMap<String, Option<String>>;

fn scratch(tag: &str) -> PathBuf {
    let dir = PathBuf::from(temp_path!("salvage-files-{tag}"));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// What `btrfs restore -i` writes out, read back on the host.
fn restored(image: &Path, out: &Path) -> Listing {
    std::fs::create_dir_all(out).unwrap();
    let run = oracle("btrfs")
        .args(["restore", "-i"])
        .arg(image)
        .arg(out)
        .output();
    assert!(
        run.status.success(),
        "btrfs restore -i {}:\n{}",
        image.display(),
        String::from_utf8_lossy(&run.stderr)
    );
    let mut listing = Listing::new();
    let mut stack = vec![out.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = path
                .strip_prefix(out)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                listing.insert(name, None);
                stack.push(path);
            } else if kind.is_file() {
                listing.insert(name, Some(sha256_file(&path).unwrap()));
            }
        }
    }
    listing
}

/// The filesystem tree's inodes, through the current root tree, or the
/// newest older one that still names the filesystem tree.
fn gathered(device: &FileDevice) -> (Layout, RootPointer, InodeScan) {
    let copies = superblock_copies(device);
    let sb = best_copy(&copies).unwrap().superblock.clone().unwrap();
    let layout = Layout::new(&sb, build_chunk_map(device, &sb).map);
    for candidate in root_tree_candidates(&copies) {
        let (roots, _) = read_root_items(device, &layout, candidate.tree_root);
        if let Some(fs) = roots.iter().find(|r| r.objectid == 5 && r.key_offset == 0) {
            let scan = collect_inodes(device, &layout, fs.pointer());
            return (layout, fs.pointer(), scan);
        }
    }
    panic!("no root tree names the filesystem tree");
}

/// What salvage recovers: every directory and regular file reachable from
/// the root directory through directory entries, each file's bytes copied
/// from its extents.
fn salvaged(device: &FileDevice, layout: &Layout, scan: &InodeScan) -> Listing {
    let mut listing = Listing::new();
    let mut children: BTreeMap<u64, Vec<(Vec<u8>, u64)>> = BTreeMap::new();
    for record in scan.inodes.values() {
        for claim in record.names.iter().filter(|c| c.from_directory) {
            children
                .entry(claim.parent)
                .or_default()
                .push((claim.name.clone(), record.ino));
        }
    }
    let mut stack = vec![(ROOT_DIR, String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for (name, ino) in children.get(&dir).cloned().unwrap_or_default() {
            let path = if prefix.is_empty() {
                String::from_utf8_lossy(&name).into_owned()
            } else {
                format!("{prefix}/{}", String::from_utf8_lossy(&name))
            };
            let Some(record) = scan.inodes.get(&ino) else {
                continue;
            };
            let Some(inode) = &record.inode else {
                continue;
            };
            match inode.mode & S_IFMT {
                S_IFDIR => {
                    listing.insert(path.clone(), None);
                    stack.push((ino, path));
                }
                S_IFREG => {
                    let mut bytes = Vec::new();
                    copy_file(device, layout, record, &mut bytes).unwrap();
                    listing.insert(path, Some(sha256_hex(&bytes)));
                }
                _ => {}
            }
        }
    }
    listing
}

/// Every file and directory on kernel-made volumes, with inline, sparse,
/// nested, and zlib, LZO and zstd compressed files, is what restore writes.
#[test]
fn healthy_volumes_recover_what_restore_recovers() {
    for name in [
        "btrfs-rich.img",
        "btrfs-comp-zlib.img",
        "btrfs-comp-lzo.img",
        "btrfs-comp-zstd.img",
    ] {
        let image = fixture(name);
        let device = FileDevice::open(&image).unwrap();
        let (layout, _, scan) = gathered(&device);
        assert!(
            scan.walk.damaged.is_empty(),
            "{name}: {:?}",
            scan.walk.damaged
        );
        let ours = salvaged(&device, &layout, &scan);
        let theirs = restored(&image, &scratch(name).join("restore"));
        assert!(
            theirs.values().filter(|v| v.is_some()).count() >= 3,
            "{name}: restore wrote files"
        );
        assert_eq!(ours, theirs, "{name}");
    }
}

/// A leaf of the twenty-thousand-file tree broken in every copy: what
/// restore still writes out, salvage recovers with the same bytes, and
/// salvage recovers at least as many files.
#[test]
fn with_a_broken_leaf_salvage_recovers_at_least_what_restore_does() {
    let name = "btrfs-deep4k.img";
    let dir = scratch("broken-leaf");
    let image = dir.join(name);
    copy_sparse(fixture(name), &image).unwrap();

    let device = FileDevice::open(&image).unwrap();
    let (layout, fs, scan) = gathered(&device);
    let healthy = salvaged(&device, &layout, &scan);
    let mut leaves = Vec::new();
    walk_tree(&device, &layout, fs, &mut |leaf| {
        leaves.push(leaf.logical);
        ControlFlow::Continue(())
    });
    assert!(leaves.len() > 3);
    let leaf = leaves[leaves.len() / 2];
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image)
        .unwrap();
    for mirror in 0..layout.copies(leaf) {
        let at = layout.chunks.map_mirror(leaf, mirror).unwrap().physical
            + u64::from(layout.geometry.nodesize)
            - 1;
        let mut byte = [0u8];
        file.read_exact_at(&mut byte, at).unwrap();
        file.write_all_at(&[byte[0] ^ 0xff], at).unwrap();
    }

    let device = FileDevice::open(&image).unwrap();
    let (layout, _, scan) = gathered(&device);
    assert_eq!(scan.walk.damaged.len(), 1);
    assert_eq!(scan.walk.damaged[0].logical, leaf);
    let ours = salvaged(&device, &layout, &scan);
    let theirs = restored(&image, &dir.join("restore"));

    // Restore writes a file it cannot fully read as what it could read,
    // often empty. Judged against the healthy volume, salvage has to be
    // right wherever restore is, and right at least as often.
    let mut ours_right = 0;
    let mut theirs_right = 0;
    for (path, truth) in &healthy {
        let ours_ok = ours.get(path) == Some(truth);
        let theirs_ok = theirs.get(path) == Some(truth);
        assert!(
            ours_ok || !theirs_ok,
            "{path}: restore recovered it and salvage did not"
        );
        ours_right += usize::from(ours_ok);
        theirs_right += usize::from(theirs_ok);
    }
    assert!(
        theirs_right < healthy.len(),
        "the damage cost restore something"
    );
    assert!(ours_right >= theirs_right);
    assert!(
        ours_right + 2 >= healthy.len(),
        "salvage recovers all but what the damaged item held: {ours_right} of {}",
        healthy.len()
    );
}
