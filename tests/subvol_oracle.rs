//! The subvolumes this driver finds must be the ones btrfs-progs
//! reports for the same filesystem.
//!
//! A subvolume listing is easy to produce and hard to produce
//! *correctly*. Walking the root tree and collecting every `ROOT_ITEM`
//! gives a list that looks right and contains the filesystem's internal
//! trees; building paths from names without following parents gives
//! `inner` where the answer is `sub/inner`; and treating a snapshot as
//! an ordinary subvolume gives a listing that is wrong only in the
//! column nobody checks.
//!
//! So the oracle is `btrfs subvolume list`, recorded beside the image
//! when it was built. It names every subvolume, gives its id and path,
//! and says which are snapshots — and it is what a user would compare
//! against.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-subvol-fixtures.sh`.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::subvol::FS_TREE_OBJECTID;
use fs_core::FileDevice;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// One line of `btrfs subvolume list -pcgu`, reduced to what is being
/// compared.
#[derive(Debug, PartialEq, Eq)]
struct Reported {
    id: u64,
    parent: u64,
    path: String,
}

/// Parse the manifest btrfs-progs wrote.
///
/// Its lines look like:
///
/// ```text
/// ID 256 gen 12 cgen 7 parent 5 top level 5 uuid ... path sub
/// ```
fn reference() -> Option<Vec<Reported>> {
    let text = std::fs::read_to_string(share().join("btrfs-subvol.manifest")).ok()?;
    let mut out = Vec::new();
    for line in text.lines() {
        if !line.starts_with("ID ") {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        let field = |name: &str| -> Option<String> {
            f.iter()
                .position(|w| *w == name)
                .and_then(|i| f.get(i + 1))
                .map(|s| (*s).to_string())
        };
        out.push(Reported {
            id: field("ID")?.parse().ok()?,
            parent: field("parent")?.parse().ok()?,
            // `path` is the last field, and a path may contain no spaces
            // in any filesystem this builds.
            path: field("path")?,
        });
    }
    Some(out)
}

/// Which files each subvolume holds, as the manifest recorded them.
fn contents() -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(share().join("btrfs-subvol.manifest")) else {
        return out;
    };
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("contains ") else {
            continue;
        };
        let Some((path, files)) = rest.split_once(':') else {
            continue;
        };
        out.insert(
            path.to_string(),
            files.split_whitespace().map(str::to_string).collect(),
        );
    }
    out
}

fn mount() -> Option<Filesystem> {
    let img = share().join("btrfs-subvol.img");
    if !img.exists() {
        return None;
    }
    Some(Filesystem::mount(Arc::new(FileDevice::open(&img).ok()?)).expect("mount"))
}

/// Every subvolume btrfs-progs reported, and no others.
#[test]
fn the_listing_matches_btrfs_progs() {
    let (Some(fs), Some(expected)) = (mount(), reference()) else {
        eprintln!("no subvolume fixture; build it with ./scripts/vm-build-subvol-fixtures.sh");
        return;
    };
    assert!(
        expected.len() >= 4,
        "the fixture should hold several subvolumes, not {}",
        expected.len()
    );

    let ours = fs.subvolumes().expect("list the subvolumes");

    // btrfs-progs does not list the default subvolume — it is where the
    // listing is taken from — so it is compared separately.
    let default: Vec<_> = ours.iter().filter(|s| s.is_default()).collect();
    assert_eq!(
        default.len(),
        1,
        "there is exactly one default subvolume, and it is always present"
    );
    assert_eq!(default[0].id, FS_TREE_OBJECTID);
    assert_eq!(default[0].path, "/");
    assert!(
        default[0].name.is_empty(),
        "the default subvolume has no name to have"
    );

    let mut got: Vec<Reported> = ours
        .iter()
        .filter(|s| !s.is_default())
        .map(|s| Reported {
            id: s.id,
            parent: s.parent,
            path: s.path.clone(),
        })
        .collect();
    got.sort_by_key(|r| r.id);

    let mut want = expected;
    want.sort_by_key(|r| r.id);

    assert_eq!(
        got, want,
        "the subvolumes found do not match what btrfs-progs reported.\n \
         ours: {got:#?}\n btrfs: {want:#?}"
    );

    eprintln!(
        "{} subvolumes match btrfs-progs, plus the default one: {:?}",
        got.len(),
        got.iter().map(|r| &r.path).collect::<Vec<_>>()
    );
}

/// A nested subvolume's path is its whole chain, not just its own name.
///
/// The fixture puts `inner` inside `sub` for exactly this: a listing
/// built from names alone reports `inner`, which is a different and
/// non-existent path.
#[test]
fn a_nested_subvolume_carries_its_parents_path() {
    let Some(fs) = mount() else {
        eprintln!("no subvolume fixture — skipping");
        return;
    };
    let subs = fs.subvolumes().expect("list");

    let inner = subs
        .iter()
        .find(|s| s.name == b"inner")
        .expect("the fixture has a nested subvolume");
    assert_eq!(
        inner.path, "sub/inner",
        "a nested subvolume's path is the chain, not its own name"
    );

    let sub = subs
        .iter()
        .find(|s| s.name == b"sub")
        .expect("the fixture has the parent");
    assert_eq!(inner.parent, sub.id, "and its parent is that subvolume");
}

/// Snapshots are distinguished from subvolumes, and read-only ones from
/// writable ones.
///
/// Both are columns a listing can get wrong while looking entirely
/// plausible. The fixture takes two snapshots of the same subvolume, one
/// read-only, so each flag is exercised against a near-identical
/// neighbour rather than against something obviously different.
#[test]
fn snapshots_and_read_only_are_told_apart() {
    let Some(fs) = mount() else {
        eprintln!("no subvolume fixture — skipping");
        return;
    };
    let subs = fs.subvolumes().expect("list");
    let by_name = |n: &[u8]| {
        subs.iter()
            .find(|s| s.name == n)
            .unwrap_or_else(|| panic!("the fixture has {}", String::from_utf8_lossy(n)))
    };

    let sub = by_name(b"sub");
    let snap = by_name(b"snap");
    let rosnap = by_name(b"rosnap");

    assert!(!sub.is_snapshot, "`sub` was created empty, not snapshotted");
    assert!(snap.is_snapshot, "`snap` is a snapshot of `sub`");
    assert!(rosnap.is_snapshot, "`rosnap` is too");

    assert!(!sub.read_only);
    assert!(!snap.read_only, "`snap` was taken writable");
    assert!(
        rosnap.read_only,
        "`rosnap` was taken with -r and is the only read-only one"
    );

    // A snapshot and its parent are different trees. They share blocks
    // when taken, but the fixture writes to `sub` afterwards, so by now
    // they must point at different roots — a driver that resolved a
    // snapshot to its parent's current tree would report the same.
    assert_ne!(
        snap.bytenr, sub.bytenr,
        "`sub` was written to after `snap` was taken, so their trees have diverged"
    );

    eprintln!(
        "snapshot and read-only flags agree with how the fixture was built \
         (sub {}, snap {}, rosnap {})",
        sub.bytenr, snap.bytenr, rosnap.bytenr
    );
}

/// The listing does not report the filesystem's internal trees.
///
/// The root tree of an ordinary filesystem holds trees numbered
/// negatively, which read as enormous unsigned values — so a filter with
/// only a lower bound admits them and reports an internal tree as a
/// subvolume.
#[test]
fn internal_trees_stay_out_of_the_listing() {
    let Some(fs) = mount() else {
        eprintln!("no subvolume fixture — skipping");
        return;
    };

    let all = fs.root_tree_items().expect("walk the root tree");
    let root_items = all.iter().filter(|(_, t, _, _)| *t == 132).count();
    let listed = fs.subvolumes().expect("list").len();

    assert!(
        root_items > listed,
        "the root tree holds {root_items} trees and the listing reports {listed}; \
         if they are equal the internal trees are being reported as subvolumes"
    );

    for s in fs.subvolumes().expect("list") {
        assert!(
            s.id == FS_TREE_OBJECTID || s.id >= 256,
            "id {} is not in the range a subvolume is numbered from",
            s.id
        );
        assert!(
            s.id < u64::MAX - 255,
            "id {} is a negatively numbered internal tree",
            s.id
        );
    }

    eprintln!("{root_items} trees in the root tree, {listed} of them subvolumes");
}

/// Each subvolume's tree really is distinct, and the manifest says what
/// each should hold.
///
/// This does not yet read *inside* a subvolume — that needs the
/// filesystem to be re-rooted at the subvolume's tree, which is the next
/// piece. What it checks is that the roots differ, which is the
/// precondition for that being worth doing at all.
#[test]
fn every_subvolume_has_a_root_of_its_own() {
    let Some(fs) = mount() else {
        eprintln!("no subvolume fixture — skipping");
        return;
    };
    let subs = fs.subvolumes().expect("list");
    let expected = contents();
    if expected.is_empty() {
        eprintln!("the manifest recorded no contents — skipping");
        return;
    }

    let mut seen: BTreeMap<u64, u64> = BTreeMap::new();
    for s in &subs {
        if let Some(other) = seen.insert(s.bytenr, s.id) {
            panic!(
                "subvolumes {} and {} share a root block ({}), so one of them is \
                 being resolved to the other's tree",
                other, s.id, s.bytenr
            );
        }
        assert_ne!(s.bytenr, 0, "subvolume {} has no root block", s.id);
    }

    eprintln!(
        "{} subvolumes, {} distinct roots; the manifest expects {:?}",
        subs.len(),
        seen.len(),
        expected.keys().collect::<Vec<_>>()
    );
}
