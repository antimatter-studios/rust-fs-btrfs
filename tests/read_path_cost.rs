//! What a read costs, in calls to the device.
//!
//! # Why this is a test and not a benchmark
//!
//! The number that matters is not wall time. Wall time on a laptop with
//! a warm page cache says more about the laptop than the driver: run it
//! twice and the second is faster for reasons this repository does not
//! control. **Calls to the device** are deterministic — the same image
//! walked the same way makes the same calls every time — so they can be
//! asserted on, and a change that makes the driver ask for more is a
//! regression a test can catch rather than a number somebody has to
//! remember.
//!
//! Wall time is printed beside them, because it is what a user feels,
//! and ignored by the assertions.
//!
//! # What is measured
//!
//! Four shapes, because they cost differently and a change can improve
//! one while ruining another:
//!
//! - **mount** — opening the filesystem. This driver loads every item
//!   of the fs tree at mount and answers from memory afterwards, so
//!   unlike its siblings almost the entire metadata cost lands here.
//!   A figure that grows with the size of the volume rather than with
//!   the work asked of it belongs to this line.
//! - **walk** — every directory in the tree, listed. Metadata only.
//! - **stat** — every file resolved by path from the root. Metadata,
//!   repeatedly, over the same nodes.
//! - **read** — every file's contents. Data, and the extent items that
//!   locate it.
//!
//! The eager load is why `walk` and `stat` reach the device at all
//! only through `mount`: the nodes every path would descend were read
//! before the first call.
//!
//! Fixtures are gitignored, so this skips on a fresh clone.

use fs_btrfs::Filesystem;
use fs_core::{CountingDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// What one shape of read cost.
struct Cost {
    reads: u64,
    bytes: u64,
    micros: u128,
    /// How much work was actually done, so a number that fell because
    /// the driver did less is not read as a number that fell because
    /// the driver got better.
    items: usize,
}

/// What one pass measured, so the two can be compared rather than each
/// asserting on itself.
struct Pass {
    mount: Cost,
    walk: Cost,
    stat: Cost,
    read: Cost,
}

fn fixture() -> Option<PathBuf> {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    // `rich` first: a varied tree is a better shape for a walk than
    // 20,000 files in one directory, which measures one leaf scan
    // repeated rather than a descent.
    for name in ["btrfs-deep4k.img", "btrfs-rich.img", "btrfs-commit.img"] {
        let p = share.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// The counter sits BELOW the cache, so what it reports is what
/// actually reached the device rather than what the driver asked for.
/// `blocks` of zero mounts without a cache, which is the baseline.
fn mount_counting(img: &Path, blocks: usize) -> (Filesystem, Arc<CountingDevice>, Cost) {
    let file = FileDevice::open(img).expect("open the fixture");
    let counting = Arc::new(CountingDevice::new(Arc::new(file)));
    let start = Instant::now();
    let fs = Filesystem::mount_with_cache(counting.clone(), blocks).expect("mount");
    let cost = Cost {
        reads: counting.reads(),
        bytes: counting.bytes(),
        micros: start.elapsed().as_micros(),
        items: 1,
    };
    (fs, counting, cost)
}

/// Every path in the tree, directories first, depth- and count-bounded
/// so a fixture with 20,000 files in one directory cannot make this run
/// for minutes.
fn walk_paths(fs: &Filesystem, at: &str, depth: u32, out: &mut Vec<(String, bool)>) {
    if depth == 0 || out.len() > 400 {
        return;
    }
    let Ok(dir) = fs.lookup_path(at) else { return };
    let Ok(entries) = fs.read_dir(dir.ino) else {
        return;
    };
    for e in entries {
        if out.len() > 400 {
            return;
        }
        let name = String::from_utf8_lossy(&e.name).to_string();
        let child = if at == "/" {
            format!("/{name}")
        } else {
            format!("{at}/{name}")
        };
        let Ok(inode) = fs.lookup_path(&child) else {
            continue;
        };
        let is_dir = inode.is_dir();
        out.push((child.clone(), is_dir));
        if is_dir {
            walk_paths(fs, &child, depth - 1, out);
        }
    }
}

fn measure<F>(counting: &CountingDevice, items: usize, body: F) -> Cost
where
    F: FnOnce(),
{
    counting.reset();
    let start = Instant::now();
    body();
    Cost {
        reads: counting.reads(),
        bytes: counting.bytes(),
        micros: start.elapsed().as_micros(),
        items,
    }
}

fn report(what: &str, c: &Cost) {
    let per = if c.items == 0 {
        0.0
    } else {
        c.reads as f64 / c.items as f64
    };
    eprintln!(
        "{what:<6} {:>6} reads  {:>9} bytes  {:>8} µs  over {:>4} items  ({per:.1} reads/item)",
        c.reads, c.bytes, c.micros, c.items
    );
}

/// The measurement itself. Prints the numbers and asserts only that the
/// driver did the work and that the cache did not make it ask for more
/// — the figures are recorded in `docs/read-path-cost.md` and compared
/// by hand when something changes, because a threshold baked in here
/// would either be so loose it catches nothing or so tight it fails on
/// a fixture rebuild.
#[test]
fn what_a_read_costs_in_calls_to_the_device() {
    let Some(img) = fixture() else {
        eprintln!("no fixture to measure — skipping");
        return;
    };
    eprintln!("measuring {}", img.display());

    eprintln!("--- uncached ---");
    let uncached = measure_one(&img, 0);
    eprintln!("--- cached ---");
    let cached = measure_one(&img, 512);

    // THE ASSERTIONS ARE ON THE UNCACHED PASS, because it is the one
    // that must reach the device: if the counter reports nothing there,
    // it is not wired to the mount and every figure above is fiction.
    // The cached pass is allowed to reach zero, so asserting the same
    // of it would be asserting that the cache failed. `walk` and `stat`
    // reach zero even uncached here, because the mount loaded the items
    // they need -- which is the finding rather than a broken counter.
    assert!(
        uncached.walk.items > 0,
        "the fixture had nothing to walk — the measurement is of nothing"
    );
    assert!(
        uncached.mount.reads > 0,
        "no calls reached the device, so the counter is not wired to the mount"
    );
    for (what, un, ca) in [
        ("mount", &uncached.mount, &cached.mount),
        ("walk", &uncached.walk, &cached.walk),
        ("stat", &uncached.stat, &cached.stat),
        ("read", &uncached.read, &cached.read),
    ] {
        // `mount` is exempt from the comparison. A cache sized in
        // sectors splits one node-sized read into four, so the cached
        // mount legitimately makes MORE calls -- which is the finding
        // that decided `DEFAULT_CACHE_BLOCKS`, not a regression.
        if what != "mount" {
            assert!(
                ca.reads <= un.reads,
                "{what}: the cache made it ask for more ({} vs {})",
                ca.reads,
                un.reads
            );
        }
        assert_eq!(
            ca.items, un.items,
            "{what}: the two passes did different amounts of work, so the \
             figures are not comparable"
        );
    }
}

fn measure_one(img: &Path, blocks: usize) -> Pass {
    let (fs, counting, mount) = mount_counting(img, blocks);
    report("mount", &mount);

    let mut paths = Vec::new();
    let walk = measure(&counting, 0, || walk_paths(&fs, "/", 8, &mut paths));
    let walk = Cost {
        items: paths.len(),
        ..walk
    };
    report("walk", &walk);

    let files: Vec<String> = paths
        .iter()
        .filter(|(_, is_dir)| !*is_dir)
        .map(|(p, _)| p.clone())
        .collect();

    // RESOLVING THE SAME PREFIXES AGAIN AND AGAIN is the shape a cache
    // is for: every path here descends from the root of the filesystem
    // tree through the same interior nodes.
    let stat = measure(&counting, files.len(), || {
        for p in &files {
            let _ = fs.lookup_path(p);
        }
    });
    report("stat", &stat);

    let read = measure(&counting, files.len(), || {
        for p in &files {
            let _ = fs.read_path(p);
        }
    });
    report("read", &read);

    Pass {
        mount,
        walk,
        stat,
        read,
    }
}
