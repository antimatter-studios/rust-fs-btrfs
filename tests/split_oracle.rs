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

mod common;
use common::{le32, le64};

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
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

/// A real split: the leaf that divided, and the two it became.
struct RealSplit {
    /// Everything that ended up in the two halves, in key order — the
    /// list the kernel had in hand at the moment it split.
    all: Vec<OwnedItem>,
    /// How many items the kernel put on the left.
    kernel_left: usize,
    nodesize: usize,
}

fn real_split(pair: &str) -> Option<RealSplit> {
    let before = share().join(format!("btrfs-split{pair}-before.img"));
    let after = share().join(format!("btrfs-split{pair}-after.img"));
    if !before.exists() || !after.exists() {
        return None;
    }
    let nodesize = {
        let dev = Arc::new(FileDevice::open(&after).ok()?);
        Filesystem::mount(dev).ok()?.superblock().nodesize as usize
    };

    let source = live_fs_leaves(&before)?;
    let result = live_fs_leaves(&after)?;

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

    let mut all = result[at].clone();
    all.extend_from_slice(&result[at + 1]);
    assert!(
        all.len() > source[at].len(),
        "{pair}: a split adds the item that caused it"
    );

    Some(RealSplit {
        all,
        kernel_left: result[at].len(),
        nodesize,
    })
}

/// Splitting a leaf the kernel split gives two valid leaves.
///
/// Valid, not identical. The split point is NOT part of the on-disk
/// format — a leaf divided somewhere else is a different, equally
/// correct tree — and the kernel's own boundary varies with the slot the
/// new item is going into, which these fixtures underdetermine. So what
/// is checked is what actually has to hold: both halves non-empty, in
/// key order, together exactly the input, and each fitting in a block.
///
/// The input is real: the items of a leaf the kernel genuinely split,
/// including the one that caused it.
fn check(pair: &str) -> Option<(usize, usize, usize)> {
    let real = real_split(pair)?;
    let (left, right) = split(&real.all).expect("splitting");

    assert!(
        !left.is_empty() && !right.is_empty(),
        "{pair}: a half with nothing in it is not a leaf the tree has a use for"
    );

    // Together, exactly the input — nothing lost, nothing invented.
    let mut rejoined = left.clone();
    rejoined.extend_from_slice(&right);
    assert_eq!(
        rejoined, real.all,
        "{pair}: the two halves are not the items that went in"
    );

    // In key order, on both sides and across the boundary, because a
    // search bisects on it.
    let key = |i: &OwnedItem| (i.key.objectid, i.key.key_type, i.key.offset);
    for half in [&left, &right] {
        for w in half.windows(2) {
            assert!(
                key(&w[0]) < key(&w[1]),
                "{pair}: a half is out of order at {:?}",
                w[0].key
            );
        }
    }
    assert!(
        key(left.last().unwrap()) < key(&right[0]),
        "{pair}: the boundary is out of order — the left half ends at {:?} and the right \
         begins at {:?}",
        left.last().unwrap().key,
        right[0].key
    );

    // Each half fits, which is the point of splitting at all.
    for (side, half) in [("left", &left), ("right", &right)] {
        let need = HEADER_SIZE + half.len() * 25 + half.iter().map(|i| i.data.len()).sum::<usize>();
        assert!(
            need <= real.nodesize,
            "{pair}: the {side} half needs {need} bytes and a leaf holds {}",
            real.nodesize
        );
    }

    Some((left.len(), right.len(), real.kernel_left))
}

/// A real split divides into two valid leaves.
#[test]
fn splitting_a_leaf_the_kernel_split_gives_two_valid_leaves() {
    let Some((l, r, kernel)) = check("") else {
        eprintln!("no split fixture; build it with ./scripts/vm-build-split-fixtures.sh");
        return;
    };
    eprintln!("even item sizes: {l} | {r}  (the kernel put {kernel} on the left)");
}

/// And so does one whose items are wildly uneven.
///
/// Items from 12 to 232 bytes. A split written to divide the BYTES
/// evenly rather than the items would land in a different place here,
/// and both must still be valid — which is the claim being made.
#[test]
fn splitting_a_leaf_of_uneven_items_gives_two_valid_leaves() {
    let Some((l, r, kernel)) = check("-vary") else {
        eprintln!("no varied split fixture — skipping");
        return;
    };
    eprintln!("uneven item sizes: {l} | {r}  (the kernel put {kernel} on the left)");
}

/// What the kernel's own boundaries were, recorded rather than asserted.
///
/// Three splits have been measured and they do not follow one simple
/// rule: 42 items became 22|20, 32 became 17|15, and 52 became 26|26.
/// This test prints what the fixtures at hand show, so that a future
/// change to the fixtures surfaces new evidence instead of quietly
/// producing the same numbers.
#[test]
fn the_kernels_own_boundary_is_recorded() {
    let mut seen = 0usize;
    for pair in ["", "-vary"] {
        let Some(real) = real_split(pair) else {
            continue;
        };
        let total = real.all.len();
        eprintln!(
            "kernel: {total} items -> {} | {}   (half is {}, half+1 is {})",
            real.kernel_left,
            total - real.kernel_left,
            total / 2,
            total / 2 + 1
        );
        // Whatever the rule, it is near the middle: a boundary in the
        // first or last fifth would mean the fixtures are not capturing
        // what this thinks they are.
        assert!(
            real.kernel_left > total / 5 && real.kernel_left < total * 4 / 5,
            "the kernel put {} of {total} items on the left, which is not a division \
             near the middle at all",
            real.kernel_left
        );
        seen += 1;
    }
    if seen == 0 {
        eprintln!("no split fixture — skipping");
    }
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
