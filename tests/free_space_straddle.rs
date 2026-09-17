//! A transaction on a block group whose free-space records span two leaves
//! leaves a free-space tree `btrfs check` accepts, or is refused (#177).
//!
//! `apply_free_space` rewrote a group's records from the leaf holding its
//! FREE_SPACE_INFO item, up to that leaf's end. A leaf can end among a
//! group's records. The first leaf then got every run of the group while the
//! next kept its old extents for the same range, and a rewrite of that next
//! leaf copied them unchanged, because it holds no INFO item.
//!
//! The image is `fst/btrfs-fst-straddle.img` from
//! `scripts/build-fst-straddle-fixture.sh`, fragmented by the kernel.
//! `scripts/fst-straddle.py` names a metadata group whose extent records
//! straddle a leaf, reading btrfs-progs' own dump. A filesystem-tree block
//! inside that group is dirtied, so the transaction releases and allocates
//! there, and the result is committed with the free-space tree kept valid.
//! `btrfs check --readonly` then compares that tree with the extent tree.
//! Skips when the fixture is missing, unless `BTRFS_ORACLE_FIXTURES=required`.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::Commit;
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> Option<PathBuf> {
    let p = manifest().join(".vm-share/fst/btrfs-fst-straddle.img");
    if p.exists() {
        return Some(p);
    }
    assert!(
        std::env::var("BTRFS_ORACLE_FIXTURES").as_deref() != Ok("required"),
        "BTRFS_ORACLE_FIXTURES=required, but {} is missing",
        p.display()
    );
    eprintln!("skip: {} not built", p.display());
    None
}

/// `(start, length)` of a metadata group whose records straddle a leaf.
fn straddling_group(image: &Path) -> (u64, u64) {
    let out = Command::new("python3")
        .arg(manifest().join("scripts/fst-straddle.py"))
        .arg(image)
        .output()
        .expect("python3");
    assert!(
        out.status.success(),
        "the fixture no longer has a metadata group whose free-space records straddle a leaf"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut words = text.split_whitespace().map(|w| w.parse::<u64>().unwrap());
    (words.next().unwrap(), words.next().unwrap())
}

/// A filesystem-tree block inside `[start, start + length)`.
fn fs_tree_block_in(image: &Path, start: u64, length: u64) -> u64 {
    let out = Command::new("btrfs")
        .args(["inspect-internal", "dump-tree", "-t", "5"])
        .arg(image)
        .output()
        .expect("btrfs-progs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            match w.next() {
                Some("leaf" | "node") => w.next()?.parse::<u64>().ok(),
                _ => None,
            }
        })
        .find(|&b| b >= start && b < start + length)
        .expect("the straddling group holds no filesystem-tree block to dirty")
}

#[test]
fn a_transaction_on_a_straddling_group_keeps_the_free_space_tree_valid() {
    let Some(source) = fixture() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("btrfs-fst-straddle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("fs.img");
    std::fs::copy(&source, &image).unwrap();

    let (start, length) = straddling_group(&image);
    let dirty = fs_tree_block_in(&image, start, length);

    let outcome = {
        let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
        let generation = fs.superblock().generation + 1;
        fs.plan_transaction_closed(&[dirty], 64).and_then(|plan| {
            let blocks = fs.render_plan(&plan, generation)?;
            let root = fs
                .planned_root(&plan)
                .expect("the plan moves the root tree");
            fs.commit(
                &blocks,
                &Commit {
                    generation,
                    root,
                    invalidate_free_space_tree: false,
                    ..Default::default()
                },
            )
        })
    };
    match outcome {
        Ok(()) => {
            let check = Command::new("btrfs")
                .args(["check", "--readonly"])
                .arg(&image)
                .output()
                .expect("btrfs-progs");
            assert!(
                check.status.success(),
                "a transaction on the group at {start} (records straddling a leaf) left a volume \
                 btrfs check rejects:\n{}{}",
                String::from_utf8_lossy(&check.stdout),
                String::from_utf8_lossy(&check.stderr)
            );
        }
        Err(e) => {
            let why = format!("{e:?}");
            assert!(
                why.contains("free-space records") && why.contains("span"),
                "refused, but not for the straddle: {why}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
