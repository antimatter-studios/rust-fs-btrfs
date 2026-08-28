//! Reproducing the split the kernel made.
//!
//! `scripts/build-split-fixtures.sh` captures a filesystem either side
//! of one leaf splitting. So the leaf that split is on disk, and so are
//! the two it became — which makes this checkable rather than a matter
//! of policy: given the items that were in the leaf plus the ones the
//! new file added, [`fs_btrfs::leaf_edit::split`] must produce the two
//! lists the kernel produced.
//!
//! Two pairs are used, and the second is the one that means anything.
//! With items of equal size, splitting at half the item count and
//! splitting at half the bytes give the same answer; the `-vary` pair is
//! built with item sizes from 12 to 232 bytes so the two rules disagree,
//! and the kernel's answer picks one.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-split-fixtures.sh`.

use fs_btrfs::btree::{header_offsets as o, HEADER_SIZE};
use fs_btrfs::chunk::DiskKey;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::leaf_edit::{split, OwnedItem};
use fs_core::FileDevice;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn items_of(block: &[u8]) -> Vec<OwnedItem> {
    let n = le32(block, o::NRITEMS) as usize;
    (0..n)
        .filter_map(|i| {
            let at = HEADER_SIZE + i * 25;
            let start = HEADER_SIZE + le32(block, at + 17) as usize;
            let end = start + le32(block, at + 21) as usize;
            (end <= block.len()).then(|| OwnedItem {
                key: DiskKey {
                    objectid: le64(block, at),
                    key_type: block[at + 8],
                    offset: le64(block, at + 9),
                },
                data: block[start..end].to_vec(),
            })
        })
        .collect()
}

/// The fs tree's LIVE leaves, in key order.
///
/// Walked from the root rather than found by scanning. Scanning turns
/// up leaves of the right generation that nothing points at any more,
/// and pairing those with each other is how the first version of this
/// test came to compare a 43-item "split" that never happened.
fn live_fs_leaves(path: &Path) -> Option<Vec<Vec<OwnedItem>>> {
    const ROOT_ITEM_KEY: u8 = 132;
    const ROOT_ITEM_BYTENR: usize = 176;

    let dev = Arc::new(FileDevice::open(path).ok()?);
    let fs = Filesystem::mount(dev).ok()?;

    // The fs tree's root, from the root tree.
    let root =
        fs.root_tree_items()
            .ok()?
            .into_iter()
            .find_map(|(objectid, key_type, _, data)| {
                (objectid == 5 && key_type == ROOT_ITEM_KEY && data.len() >= ROOT_ITEM_BYTENR + 8)
                    .then(|| le64(&data, ROOT_ITEM_BYTENR))
            })?;

    // Depth-first, left to right, so the leaves come out in key order.
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(at) = stack.pop() {
        let Ok(block) = fs.read_tree_block(at) else {
            continue;
        };
        match block.body.key_ptrs() {
            Some(ptrs) => {
                for p in ptrs.iter().rev() {
                    stack.push(p.blockptr);
                }
            }
            None => out.push(items_of(block.bytes())),
        }
    }
    Some(out)
}

/// Given the leaf that split and the leaves it became, reproduce them.
fn check(pair: &str) -> Option<(usize, usize)> {
    let before = share().join(format!("btrfs-split{pair}-before.img"));
    let after = share().join(format!("btrfs-split{pair}-after.img"));
    if !before.exists() || !after.exists() {
        return None;
    }

    let source = live_fs_leaves(&before)?;
    let result = live_fs_leaves(&after)?;

    // One leaf became two, so the after tree has exactly one more, and
    // the pair that replaced it is at the position where the two lists
    // first differ.
    assert_eq!(
        result.len(),
        source.len() + 1,
        "{pair}: the fs tree went from {} live leaves to {}. A single split adds exactly \
         one, so this pair does not hold the event it is supposed to.",
        source.len(),
        result.len()
    );
    let at = (0..source.len())
        .find(|&i| source[i] != result[i])
        .unwrap_or(source.len() - 1);

    let (kernel_left, kernel_right) = (&result[at], &result[at + 1]);

    // Everything that ended up in the two halves, in order — which is
    // the list the kernel had in hand at the moment it split.
    let mut all = kernel_left.clone();
    all.extend_from_slice(kernel_right);

    assert!(
        all.len() > source[at].len(),
        "{pair}: the two leaves after hold {} items and the one before held {}. A split \
         adds the item that caused it.",
        all.len(),
        source[at].len()
    );

    let (ours_left, ours_right) = split(&all).expect("splitting");

    assert_eq!(
        ours_left.len(),
        kernel_left.len(),
        "{pair}: the kernel put {} items on the left of a {}-item split and this puts {}. \
         Left holds {} bytes of data, right {}.",
        kernel_left.len(),
        all.len(),
        ours_left.len(),
        kernel_left.iter().map(|i| i.data.len()).sum::<usize>(),
        kernel_right.iter().map(|i| i.data.len()).sum::<usize>()
    );
    assert_eq!(
        ours_left, *kernel_left,
        "{pair}: the left half holds different items from the kernel's"
    );
    assert_eq!(
        ours_right, *kernel_right,
        "{pair}: the right half holds different items from the kernel's"
    );

    Some((ours_left.len(), ours_right.len()))
}

/// The rule reproduces a real split.
#[test]
fn the_split_matches_the_one_the_kernel_made() {
    let Some((l, r)) = check("") else {
        eprintln!("no split fixture; build it with ./scripts/vm-build-split-fixtures.sh");
        return;
    };
    eprintln!("even item sizes: {l} | {r}");
}

/// And reproduces one where the two candidate rules disagree.
///
/// This is the test that has any force. With items of equal size,
/// half-the-count and half-the-bytes give the same boundary, so the
/// pair above cannot tell them apart. Here the items run from 12 to 232
/// bytes and the two rules differ by two positions.
#[test]
fn the_split_matches_when_the_items_are_uneven() {
    let Some((l, r)) = check("-vary") else {
        eprintln!("no varied split fixture — skipping");
        return;
    };

    // And say what the losing rule would have done, so a future change
    // that quietly switches to it is visible in the output.
    eprintln!("uneven item sizes: {l} | {r} — half the bytes would not have put it there");
}

/// A leaf too small to split is refused rather than producing an empty
/// half.
#[test]
fn a_leaf_with_nothing_to_divide_is_refused() {
    let one = vec![OwnedItem {
        key: DiskKey {
            objectid: 1,
            key_type: 1,
            offset: 0,
        },
        data: vec![0; 8],
    }];
    assert!(split(&one).is_err(), "one item cannot be split in two");
    assert!(split(&[]).is_err(), "nor can none");
}
