//! A filesystem sprouted from a seed is read across both devices, and the
//! seed by itself as the filesystem it was (#270).
//!
//! `btrfstune -S 1` makes a filesystem a seed; `btrfs device add` on its
//! read-only mount grows a new filesystem over it, with a new fsid, whose
//! chunks are partly the seed's and partly the new device's. The seed
//! device's own superblock still names the seed's fsid, so a reader that
//! pairs a pool's devices by fsid refuses the very device the sprout
//! needs.
//!
//! The fixtures are made by the kernel in the harness VM (`chore
//! fixtures`, target `seed`): the seed, filled and then marked; the device
//! sprouted from it, with new files and one seed file rewritten. Each
//! manifest is every file's size and SHA-256 as the kernel read it, the
//! seed's before sprouting and the sprout's after.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{fixture, sha256_hex};
use fs_core::{BlockRead, FileDevice};
use std::sync::Arc;

fn device(name: &str) -> Arc<dyn BlockRead> {
    let path = fixture(&format!("seed/{name}"));
    Arc::new(FileDevice::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// `(path, size, sha256)` for every file in a manifest.
fn manifest(name: &str) -> Vec<(String, usize, String)> {
    let path = fixture(&format!("seed/{name}"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let files: Vec<(String, usize, String)> = text
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 3, "a manifest line: {line:?}");
            (
                f[0].to_string(),
                f[1].parse().expect("a size"),
                f[2].to_string(),
            )
        })
        .collect();
    assert!(files.len() >= 4, "{name} names {} files", files.len());
    files
}

fn reads_every_file(fs: &Filesystem, manifest_name: &str, what: &str) {
    for (path, size, digest) in manifest(manifest_name) {
        let got = fs
            .read_path(&path)
            .unwrap_or_else(|e| panic!("{what}: reading {path}: {e}"));
        assert_eq!(got.len(), size, "{what}: {path}: length");
        assert_eq!(
            sha256_hex(&got),
            digest,
            "{what}: {path}: {size} bytes read, but not the bytes the kernel wrote"
        );
    }
}

#[test]
fn the_sprouted_filesystem_reads_every_file_across_seed_and_sprout() {
    let fs = Filesystem::mount_pool(vec![device("btrfs-seed.img"), device("btrfs-sprout.img")])
        .unwrap_or_else(|e| panic!("the seed and its sprout should open together: {e}"));
    reads_every_file(&fs, "btrfs-sprout.manifest", "seed + sprout");
}

#[test]
fn the_order_the_devices_are_given_in_does_not_matter() {
    let fs = Filesystem::mount_pool(vec![device("btrfs-sprout.img"), device("btrfs-seed.img")])
        .unwrap_or_else(|e| panic!("sprout + seed should open: {e}"));
    reads_every_file(&fs, "btrfs-sprout.manifest", "sprout + seed");
}

#[test]
fn the_seed_by_itself_is_the_filesystem_it_was() {
    let fs = Filesystem::mount(device("btrfs-seed.img"))
        .unwrap_or_else(|e| panic!("the seed by itself should mount: {e}"));
    reads_every_file(&fs, "btrfs-seed.manifest", "seed alone");
    let fs = Filesystem::mount_pool(vec![device("btrfs-seed.img")])
        .unwrap_or_else(|e| panic!("the seed alone as a pool should mount: {e}"));
    reads_every_file(&fs, "btrfs-seed.manifest", "seed alone, as a pool");
}

#[test]
fn the_sprout_without_its_seed_is_refused() {
    match Filesystem::mount(device("btrfs-sprout.img")) {
        Ok(_) => panic!("the sprout opened without its seed"),
        Err(e) => assert!(e.to_string().contains("spans 2 devices"), "{e}"),
    }
}

/// A seed device that is not the sprout's own is refused even when its
/// devid fits: here the sprout given the seed twice is the simplest such
/// case the fixtures allow, and a device of an unrelated filesystem in the
/// seed's place is the other.
#[test]
fn a_device_that_is_not_the_sprouts_seed_is_refused() {
    let doubled = Filesystem::mount_pool(vec![
        device("btrfs-seed.img"),
        device("btrfs-seed.img"),
        device("btrfs-sprout.img"),
    ]);
    assert!(doubled.is_err(), "the seed given twice was accepted");

    let other = Arc::new(FileDevice::open(fixture("btrfs-default.img")).expect("open"))
        as Arc<dyn BlockRead>;
    assert!(
        Filesystem::mount_pool(vec![other, device("btrfs-sprout.img")]).is_err(),
        "an unrelated filesystem's device was accepted as the seed"
    );
}
