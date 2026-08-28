//! What a planned transaction contains, checked against a real one.
//!
//! `Filesystem::plan_transaction` works out which blocks a change makes
//! the filesystem rewrite. `docs/cow-transaction.md` measured what the
//! kernel rewrote for one `touch` on the same filesystem, so the plan
//! has something to be compared against that was not written to fit it.
//!
//! The comparison is a containment, not an equality, and the reason is
//! the planner's own documented boundary: it computes the spine — the
//! dirty blocks, their ancestors, and the root tree leaf naming the
//! tree whose root moved — and does not model the extent, free-space and
//! dev tree rewrites that recording those allocations causes. So the
//! trees it names must be among the trees the kernel touched, and must
//! include the ones the spine implies.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-cow-fixtures.sh`.

use fs_btrfs::chunk::objectid;
use fs_btrfs::fs::Filesystem;
use fs_core::FileDevice;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn fixture(name: &str) -> Option<Filesystem> {
    let p = share().join(name);
    if !p.exists() {
        return None;
    }
    let dev = Arc::new(FileDevice::open(&p).ok()?);
    Filesystem::mount(dev).ok()
}

/// The fs tree's root, which is the block a change to a file reaches.
fn fs_tree_root(fs: &Filesystem) -> Option<u64> {
    /// `BTRFS_ROOT_ITEM_KEY`, and `btrfs_root_item.bytenr` within it.
    const ROOT_ITEM_KEY: u8 = 132;
    const BYTENR: usize = 176;
    fs.root_tree_items()
        .ok()?
        .into_iter()
        .find(|(objid, ty, _, data)| {
            *objid == objectid::FS_TREE && *ty == ROOT_ITEM_KEY && data.len() >= BYTENR + 8
        })
        .map(|(_, _, _, data)| u64::from_le_bytes(data[BYTENR..BYTENR + 8].try_into().unwrap()))
}

/// A plan for changing the fs tree covers the spine above it.
#[test]
fn a_change_to_the_fs_tree_rewrites_it_and_the_root_tree() {
    let Some(fs) = fixture("btrfs-cow-before.img") else {
        eprintln!("no fixtures; build them with ./scripts/vm-build-cow-fixtures.sh");
        return;
    };
    let Some(fs_root) = fs_tree_root(&fs) else {
        eprintln!("no fs tree — skipping");
        return;
    };

    let plan = fs.plan_transaction(&[fs_root]).expect("planning");
    let trees = plan.trees();

    assert!(
        trees.contains(&objectid::FS_TREE),
        "the fs tree's own root was made dirty and the plan does not rewrite it: {trees:?}"
    );
    assert!(
        trees.contains(&objectid::ROOT_TREE),
        "the fs tree's root moved, so the ROOT_ITEM naming it changed and the root tree \
         leaf holding it must be rewritten. The plan stops short: {trees:?}"
    );

    // The block that changed is in the plan, and it is going somewhere
    // else — a copy-on-write rewrite that lands on the same address is
    // an overwrite.
    let it = plan
        .rewrites
        .iter()
        .find(|r| r.old == fs_root)
        .expect("the dirty block itself must be in the plan");
    assert_ne!(
        it.new, it.old,
        "the plan rewrites {fs_root} onto itself, which is not copy-on-write"
    );

    eprintln!(
        "{} blocks, trees {:?}",
        plan.rewrites.len(),
        trees.iter().collect::<Vec<_>>()
    );
}

/// Every tree the plan names is one the kernel also rewrote.
///
/// The kernel's transaction for the same change is the upper bound: a
/// plan that touches a tree the kernel did not is rewriting something
/// for no reason, which costs a block and, once the extent tree is being
/// edited, records an allocation nothing needed.
#[test]
fn the_plan_touches_no_tree_the_kernel_left_alone() {
    let (Some(before), Some(after)) = (
        fixture("btrfs-cow-before.img"),
        fixture("btrfs-cow-after.img"),
    ) else {
        eprintln!("no fixtures — skipping");
        return;
    };
    let Some(fs_root) = fs_tree_root(&before) else {
        return;
    };

    // Which trees the kernel rewrote: any block in the after image
    // newer than the before image belongs to one.
    let old_gen = before.superblock().generation;
    let path = share().join("btrfs-cow-after.img");
    let image = std::fs::read(&path).expect("reading the after image");
    let sb = after.superblock();
    let n = sb.nodesize as usize;
    let mut kernel: BTreeSet<u64> = BTreeSet::new();
    let mut at = 0usize;
    while at + n <= image.len() {
        let b = &image[at..at + n];
        at += n;
        if b[0x20..0x30] != sb.fsid[..] {
            continue;
        }
        if !sb.csum_type.verify(&b[32..], &b[..32]) {
            continue;
        }
        let gen = u64::from_le_bytes(b[0x50..0x58].try_into().unwrap());
        if gen > old_gen {
            kernel.insert(u64::from_le_bytes(b[0x58..0x60].try_into().unwrap()));
        }
    }

    if kernel.is_empty() {
        eprintln!("the after image holds no newer block — nothing to compare against");
        return;
    }

    let plan = before.plan_transaction(&[fs_root]).expect("planning");
    for tree in plan.trees() {
        assert!(
            kernel.contains(&tree),
            "the plan rewrites tree {tree}, which the kernel did not touch for the same \
             change. It rewrote {kernel:?}."
        );
    }
    eprintln!(
        "plan touches {:?}, kernel touched {:?}",
        plan.trees().iter().collect::<Vec<_>>(),
        kernel.iter().collect::<Vec<_>>()
    );
}

/// Nothing is placed where something already is.
#[test]
fn every_new_address_is_free_distinct_and_aligned() {
    let Some(fs) = fixture("btrfs-cow-before.img") else {
        eprintln!("no fixtures — skipping");
        return;
    };
    let Some(fs_root) = fs_tree_root(&fs) else {
        return;
    };
    let nodesize = fs.superblock().nodesize as u64;

    let plan = fs.plan_transaction(&[fs_root]).expect("planning");
    assert!(!plan.rewrites.is_empty(), "an empty plan tests nothing");

    // Distinct: two blocks sharing an address is one block.
    let news: BTreeSet<u64> = plan.allocated().into_iter().collect();
    assert_eq!(
        news.len(),
        plan.rewrites.len(),
        "the plan places {} blocks at {} distinct addresses",
        plan.rewrites.len(),
        news.len()
    );

    // Free, according to the extent tree — the addresses the plan takes
    // must not be ones already holding something.
    let groups: Vec<_> = fs
        .block_groups()
        .expect("block groups")
        .into_iter()
        .filter(|g| g.holds_metadata())
        .collect();
    let free = fs.free_extents_by_group(&groups).expect("free space");
    for at in &news {
        assert_eq!(at % nodesize, 0, "{at} is not tree-block aligned");
        let covered = free
            .iter()
            .flatten()
            .any(|r| *at >= r.start && at + nodesize <= r.end());
        assert!(
            covered,
            "the plan puts a block at {at}, which the extent tree says is allocated. \
             Writing there overwrites live data and the filesystem still mounts."
        );
    }

    // One release per allocation, so usage does not move.
    assert_eq!(
        plan.usage_delta(nodesize),
        0,
        "a plan that only rewrites should not change how much is used"
    );
    eprintln!("{} placements, all free, distinct and aligned", news.len());
}

/// A block nothing points at cannot be planned around.
#[test]
fn planning_an_unreachable_block_is_refused() {
    let Some(fs) = fixture("btrfs-cow-before.img") else {
        eprintln!("no fixtures — skipping");
        return;
    };
    // An address inside the filesystem but holding no tree block.
    let nowhere = fs.superblock().root + fs.superblock().nodesize as u64 * 1_000;
    let err = fs
        .plan_transaction(&[nowhere])
        .expect_err("a block that is part of no tree has nothing above it to rewrite");
    assert!(
        err.to_string().contains("not reachable"),
        "the refusal should say why: {err}"
    );
}

/// A change to a leaf rewrites every node above it.
///
/// The other tests plan from a tree's root, which has no ancestors — so
/// they never exercise the walk upwards at all, and a planner that
/// simply did not do it passed every one of them. That is what a
/// surviving mutation looks like, and it is why this test uses a
/// filesystem deep enough to have something above a leaf.
#[test]
fn a_change_to_a_leaf_rewrites_the_nodes_above_it() {
    // The deep geometries are the ones built with enough files to push
    // a tree past a single block.
    let Some(fs) = ["btrfs-deep16k.img", "btrfs-deep4k.img"]
        .iter()
        .find_map(|n| fixture(n))
    else {
        eprintln!("no deep fixture — skipping");
        return;
    };
    let Some(fs_root) = fs_tree_root(&fs) else {
        return;
    };

    // Descend to a leaf, remembering the path. Anything with a node
    // above it will do.
    let mut path = vec![fs_root];
    while let Ok(block) = fs.read_tree_block(*path.last().unwrap()) {
        let Some(first) = block.body.key_ptrs().and_then(|p| p.first().copied()) else {
            break;
        };
        path.push(first.blockptr);
    }

    if path.len() < 2 {
        eprintln!("the fs tree is a single block — nothing above a leaf to check");
        return;
    }

    let leaf = *path.last().unwrap();
    let plan = fs.plan_transaction(&[leaf]).expect("planning");
    let rewritten: BTreeSet<u64> = plan.rewrites.iter().map(|r| r.old).collect();

    // Every block on the path from the leaf up to the tree's root.
    for at in &path {
        assert!(
            rewritten.contains(at),
            "the leaf at {leaf} was made dirty and {at} is above it on the path to the \
             root, but the plan leaves it alone. A node still pointing at the leaf's OLD \
             address is a tree that reads the version before the change."
        );
    }

    assert!(
        plan.trees().contains(&objectid::ROOT_TREE),
        "the fs tree's root moved and the root tree was not rewritten"
    );
    eprintln!(
        "a leaf {} levels down: {} blocks rewritten, whole path included",
        path.len() - 1,
        plan.rewrites.len()
    );
}
