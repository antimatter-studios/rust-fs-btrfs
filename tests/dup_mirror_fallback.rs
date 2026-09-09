//! One bad copy of a tree block does not make a DUP volume unreadable.
//!
//! # What this holds that the unit tests do not
//!
//! `Tree::read_block`'s fallback is witnessed three ways in the unit
//! tests — the retry, which error is reported when no copy verifies, and
//! that a good first copy is not read twice. None of them reaches
//! `Filesystem`. Removing all four production opt-ins together
//! (`fs.rs:717`, `:749`, `:976`, `:1640`) — that is, the driver never
//! asking for a second copy at all, which IS the filed defect — leaves
//! the whole suite green at **416 passed, 0 failed**.
//!
//! So the mechanism was held and the plumbing was not. This file is the
//! plumbing: it damages a real `mkfs.btrfs` image and requires the
//! driver to mount it.
//!
//! # Why it is cheap, having been called expensive
//!
//! The first reading of this gap was that it needed a Linux-only image
//! builder. It does not: `.vm-share/btrfs-dup.img` already ships. It is
//! gitignored, so a fresh worktree lacks it and a symlink is enough —
//! which is worth remembering before the next "no fixture available".
//! Public API only, no `mkfs.btrfs`, no VM.
//!
//! # Vacuity
//!
//! Without `.vm-share` this suite reports `ok` having done nothing —
//! the shape of every oracle suite here, and the shape that makes
//! `13 passed in 0.00s` and `13 passed in 33.61s` look identical. So it
//! prints what it did and how long the image was, and the assertions
//! below check the fixture is a DUP one before believing any pass:
//! a chunk with one copy has nothing to fall back to, and a test that
//! "passed" on it would mean nothing at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_btrfs::superblock::SUPER_INFO_OFFSET;
use fs_btrfs::{ChunkMap, Superblock};
use fs_core::FileDevice;

fn dup_image() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".vm-share")
        .join("btrfs-dup.img");
    p.exists().then_some(p)
}

/// Write `bytes` somewhere unique to this process and hand back a guard
/// that removes it, so a panic mid-test does not leave the image behind.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str, bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "btrfs-dup-{tag}-{}-{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn a_damaged_first_copy_of_the_chunk_root_does_not_stop_the_mount() {
    let Some(src) = dup_image() else {
        eprintln!(
            "no .vm-share/btrfs-dup.img — run ./scripts/vm-build-fixtures.sh; \
             skipping, and this suite proved nothing"
        );
        return;
    };
    let bytes = std::fs::read(&src).expect("read fixture");
    eprintln!(
        "dup_mirror_fallback: {} ({} bytes)",
        src.display(),
        bytes.len()
    );

    let sb = Superblock::parse_at(&bytes[SUPER_INFO_OFFSET as usize..], SUPER_INFO_OFFSET)
        .expect("superblock");
    let map = ChunkMap::bootstrap(&sb).expect("bootstrap");

    // THE HARNESS CHECKS BEFORE THE ASSERTION. A chunk with one copy has
    // nothing to fall back to, so a pass below would say nothing about
    // the fallback — it would say the damage missed.
    let addr = sb.chunk_root;
    let copies = map.mirrors_at(addr).expect("mirrors_at");
    assert!(
        copies >= 2,
        "chunk_root has {copies} copy; btrfs-dup.img is not a DUP fixture and this arm \
         cannot mean anything on it"
    );
    let first = map.map_mirror(addr, 0).expect("mirror 0");
    let second = map.map_mirror(addr, 1).expect("mirror 1");
    assert_ne!(
        first.physical, second.physical,
        "the two copies share a physical offset, so damaging one damages both"
    );

    // The control: the image mounts before anything is done to it. A
    // fixture that could not mount would make the interesting result
    // below indistinguishable from a broken image.
    {
        let pristine = Scratch::new("pristine", &bytes);
        let dev = FileDevice::open(&pristine.0).expect("open pristine");
        fs_btrfs::fs::Filesystem::mount(Arc::new(dev)).expect("the undamaged fixture must mount");
    }

    // ONE BYTE IN THE ITEM AREA OF COPY 0, past the 101-byte header — so
    // the header still claims to be this block and the checksum is the
    // only thing that catches it. That is what a bad sector produces,
    // and it is the case the fallback exists for; damaging the header
    // instead would be caught by the identity fields and prove less.
    let mut damaged = bytes.clone();
    let at = (first.physical + 200) as usize;
    damaged[at] ^= 0xFF;

    let image = Scratch::new("damaged", &damaged);
    let dev = FileDevice::open(&image.0).expect("open damaged");
    fs_btrfs::fs::Filesystem::mount(Arc::new(dev)).unwrap_or_else(|e| {
        panic!(
            "copy 0 of chunk_root at physical {} was damaged and copy 1 at {} is intact; \
             the mount must fall back to it. Got {e:?}",
            first.physical, second.physical
        )
    });
    eprintln!("dup_mirror_fallback: mounted with copy 0 of chunk_root damaged");
}

/// THE CHUNK ROOT IS ONE OF FOUR OPT-INS, AND IT ONLY REACHES ONE.
///
/// `Filesystem` asks for redundancy in four places: the chunk-tree
/// bootstrap walk, the root-tree walk, the fs-tree walk and the pool's
/// tree reader. Damaging `chunk_root` reaches the first alone —
/// measured: removing each of the four individually, only the first
/// fails the test above.
///
/// So this damages copy 0 of every tree root the superblock names that
/// has a second copy, one image per root, and requires the mount each
/// time. It asserts how many roots it actually exercised, because a
/// version that silently found one would look exactly like this one
/// passing.
#[test]
fn a_damaged_first_copy_of_any_named_root_does_not_stop_the_mount() {
    let Some(src) = dup_image() else {
        eprintln!("no .vm-share/btrfs-dup.img; skipping, and this suite proved nothing");
        return;
    };
    let bytes = std::fs::read(&src).expect("read fixture");
    let sb = Superblock::parse_at(&bytes[SUPER_INFO_OFFSET as usize..], SUPER_INFO_OFFSET)
        .expect("superblock");

    // THE FULL MAP, NOT THE BOOTSTRAP ONE. `ChunkMap::bootstrap` knows
    // only the system chunks embedded in the superblock, so it cannot
    // resolve `sb.root` at all — and a version of this test built on it
    // silently exercised one root while claiming two. Mounting the
    // pristine image builds the real map, which is also the thing under
    // test having worked.
    let pristine = Scratch::new("map", &bytes);
    let dev = FileDevice::open(&pristine.0).expect("open pristine");
    let mounted = fs_btrfs::fs::Filesystem::mount(Arc::new(dev)).expect("pristine must mount");
    let map = mounted.chunk_map();

    let mut exercised = 0usize;
    for (what, addr) in [("chunk_root", sb.chunk_root), ("root", sb.root)] {
        let Ok(copies) = map.mirrors_at(addr) else {
            continue;
        };
        if copies < 2 {
            eprintln!("dup_mirror_fallback: {what} has {copies} copy; nothing to fall back to");
            continue;
        }
        let first = map.map_mirror(addr, 0).expect("mirror 0");
        let second = map.map_mirror(addr, 1).expect("mirror 1");
        assert_ne!(first.physical, second.physical, "{what}: copies coincide");

        let mut damaged = bytes.clone();
        damaged[(first.physical + 200) as usize] ^= 0xFF;
        let image = Scratch::new(what, &damaged);
        let dev = FileDevice::open(&image.0).expect("open damaged");
        fs_btrfs::fs::Filesystem::mount(Arc::new(dev)).unwrap_or_else(|e| {
            panic!(
                "{what}: copy 0 at physical {} damaged, copy 1 at {} intact; \
                 the mount must fall back to it. Got {e:?}",
                first.physical, second.physical
            )
        });
        eprintln!("dup_mirror_fallback: mounted with copy 0 of {what} damaged");
        exercised += 1;
    }

    assert!(
        exercised >= 2,
        "only {exercised} root(s) had a second copy on this fixture, so this test \
         reached fewer opt-ins than it claims to"
    );
}

/// The other direction, and it is the one that stops the tests above
/// passing for the wrong reason: damage BOTH copies and the mount must
/// refuse. Without this, a driver that ignored checksums entirely would
/// satisfy the fallback assertion.
#[test]
fn damaging_every_copy_is_still_refused() {
    let Some(src) = dup_image() else {
        eprintln!("no .vm-share/btrfs-dup.img; skipping, and this suite proved nothing");
        return;
    };
    let bytes = std::fs::read(&src).expect("read fixture");
    let sb = Superblock::parse_at(&bytes[SUPER_INFO_OFFSET as usize..], SUPER_INFO_OFFSET)
        .expect("superblock");
    let map = ChunkMap::bootstrap(&sb).expect("bootstrap");

    let addr = sb.chunk_root;
    let copies = map.mirrors_at(addr).expect("mirrors_at");
    assert!(copies >= 2, "not a DUP fixture");

    let mut damaged = bytes.clone();
    for mirror in 0..copies {
        let m = map.map_mirror(addr, mirror).expect("mirror");
        damaged[(m.physical + 200) as usize] ^= 0xFF;
    }

    let image = Scratch::new("both", &damaged);
    let dev = FileDevice::open(&image.0).expect("open damaged");
    let err = fs_btrfs::fs::Filesystem::mount(Arc::new(dev))
        .err()
        .expect("with every copy damaged the mount must refuse, not fall back to nothing");
    eprintln!("dup_mirror_fallback: both copies damaged -> {err:?}");
}
