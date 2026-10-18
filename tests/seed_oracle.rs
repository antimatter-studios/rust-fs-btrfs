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
use fs_btrfs_test_support::{fixture, sha256_hex, temp_path};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::sync::Arc;

fn device(name: &str) -> Arc<dyn BlockRead> {
    let path = fixture(&format!("seed/{name}"));
    Arc::new(FileDevice::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// A writable copy of a fixture in the scratch directory, so the fixture
/// itself is never opened for writing.
fn writable_copy(name: &str, tag: &str) -> Arc<dyn BlockDevice> {
    let copy = std::path::PathBuf::from(temp_path!("seed-{tag}-{name}"));
    let from = fixture(&format!("seed/{name}"));
    std::fs::copy(&from, &copy).unwrap_or_else(|e| panic!("copying {}: {e}", from.display()));
    Arc::new(FileDevice::open_rw(&copy).unwrap_or_else(|e| panic!("{}: {e}", copy.display())))
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

/// The seed and its sprout are one filesystem, which `mount_pool` reads,
/// and `mount_pool_rw` opens it too (#624): the sprout is written, the seed
/// held read-only beside it. The test keeps the name it had while the pair
/// was refused (#561); the seed is still named, now as the device left
/// unwritten.
#[test]
fn the_sprouted_filesystem_given_for_writing_is_refused_naming_the_seed() {
    let fixture_seed = sha256_hex(&std::fs::read(fixture("seed/btrfs-seed.img")).expect("seed"));
    for (tag, seed_first) in [("seed-first", true), ("sprout-first", false)] {
        let seed_path = std::path::PathBuf::from(temp_path!("seed-rw-{tag}-btrfs-seed.img"));
        std::fs::copy(fixture("seed/btrfs-seed.img"), &seed_path).expect("copying the seed");
        let seed = Arc::new(FileDevice::open_rw(&seed_path).expect("open the seed copy"))
            as Arc<dyn BlockDevice>;
        let sprout = writable_copy("btrfs-sprout.img", tag);
        let devices = if seed_first {
            vec![seed, sprout]
        } else {
            vec![sprout, seed]
        };
        let fs = Filesystem::mount_pool_rw(devices)
            .unwrap_or_else(|e| panic!("{tag}: the sprouted filesystem was refused: {e}"));
        reads_every_file(&fs, "btrfs-sprout.manifest", tag);
        drop(fs);
        let after = sha256_hex(&std::fs::read(&seed_path).expect("reading the seed copy"));
        assert_eq!(after, fixture_seed, "{tag}: the seed device was written");
    }
}

/// Naming the seed does not stop an unrelated filesystem's device from
/// being refused as one.
#[test]
fn an_unrelated_device_given_for_writing_is_still_another_filesystem() {
    let default = std::path::PathBuf::from(temp_path!("seed-unrelated-btrfs-default.img"));
    std::fs::copy(fixture("btrfs-default.img"), &default).expect("copying btrfs-default.img");
    let other = Arc::new(FileDevice::open_rw(&default).expect("open")) as Arc<dyn BlockDevice>;
    match Filesystem::mount_pool_rw(vec![other, writable_copy("btrfs-sprout.img", "unrelated")]) {
        Ok(_) => panic!("an unrelated filesystem's device was accepted beside the sprout"),
        Err(e) => assert!(e.to_string().contains("different filesystems"), "{e}"),
    }
}
