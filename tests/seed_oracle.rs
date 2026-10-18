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
use fs_btrfs_test_support::{fixture, oracle, sha256_hex, temp_path};
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

/// Allocations, including the transaction's bookkeeping trees, must not
/// consume free space in the seed's otherwise ordinary block groups (#625).
#[test]
fn new_data_and_tree_blocks_land_only_on_the_sprout() {
    let seed_path = std::path::PathBuf::from(temp_path!("seed-allocation-seed.img"));
    let sprout_path = std::path::PathBuf::from(temp_path!("seed-allocation-sprout.img"));
    for (name, copy) in [
        ("btrfs-seed.img", &seed_path),
        ("btrfs-sprout.img", &sprout_path),
    ] {
        std::fs::copy(fixture(&format!("seed/{name}")), copy).expect("copy the member");
    }
    let before = sha256_hex(&std::fs::read(&seed_path).expect("read the seed"));
    let seed = Arc::new(FileDevice::open_rw(&seed_path).unwrap()) as Arc<dyn BlockDevice>;
    let sprout = Arc::new(FileDevice::open_rw(&sprout_path).unwrap()) as Arc<dyn BlockDevice>;
    let mut fs = Filesystem::mount_pool_rw(vec![seed, sprout]).expect("mount the pair");
    let sprout_id = fs.superblock().dev_item.devid;
    let generation = fs.superblock().generation;
    let old_chunks: std::collections::BTreeSet<u64> =
        fs.chunk_map().chunks().iter().map(|c| c.logical).collect();
    assert!(
        fs.chunk_map()
            .chunks()
            .iter()
            .any(|c| { c.stripes.iter().any(|s| s.devid != sprout_id) }),
        "the pair must contain seed chunks"
    );
    let at = fs.find_metadata_block().expect("free writable metadata");
    assert_on_sprout(&fs, at, sprout_id);
    let parent = fs.lookup_path("/").unwrap().ino;
    let ino = fs
        .create(parent, b"allocated-on-sprout", 0o644, 0, 0)
        .expect("create");
    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        fs.write(ino, 0, &payload).expect("write regular data"),
        payload.len()
    );
    assert_eq!(fs.read_file(ino).unwrap(), payload);
    assert!(fs.superblock().generation > generation);
    for chunk in fs
        .chunk_map()
        .chunks()
        .iter()
        .filter(|c| !old_chunks.contains(&c.logical))
    {
        assert!(
            chunk.stripes.iter().all(|s| s.devid == sprout_id),
            "new chunk on seed"
        );
    }
    let mut roots = vec![fs.superblock().root, fs.superblock().chunk_root];
    for (_, ty, _, data) in fs.root_tree_items().unwrap() {
        if ty == 132 && data.len() >= 184 {
            let root = u64::from_le_bytes(data[176..184].try_into().unwrap());
            if root != 0 {
                roots.push(root);
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut new_blocks = 0;
    let mut data_extents = 0;
    while let Some(at) = roots.pop() {
        if !seen.insert(at) {
            continue;
        }
        let block = fs.read_tree_block(at).expect("read a reachable tree block");
        if block.header.generation > generation {
            assert_on_sprout(&fs, at, sprout_id);
            new_blocks += 1;
        }
        if let Some(children) = block.body.key_ptrs() {
            roots.extend(children.iter().map(|p| p.blockptr));
        }
        if let Some(items) = block.body.items() {
            for item in items.iter().filter(|i| {
                block.header.owner == 5 && i.key.objectid == ino && i.key.key_type == 108
            }) {
                let data = block.item_data(item).unwrap();
                assert!(data.len() >= 53 && data[20] == 1, "regular file extent");
                let at = u64::from_le_bytes(data[21..29].try_into().unwrap());
                assert_on_sprout(&fs, at, sprout_id);
                data_extents += 1;
            }
        }
    }
    assert!(new_blocks >= 3, "the committed trees must have moved");
    assert!(
        data_extents > 0,
        "the new file must allocate regular extents"
    );
    drop(fs);
    assert_eq!(
        sha256_hex(&std::fs::read(&seed_path).unwrap()),
        before,
        "seed changed"
    );
    let out = oracle("bash")
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/check-pool.sh"))
        .arg(&sprout_path)
        .arg(&seed_path)
        .output();
    assert!(
        out.status.success(),
        "btrfs check of the pair: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A subvolume handle on a sprouted pool keeps the seed when it hands
/// write access back after a commit (#628): a second write through the
/// same handle still reaches the sprout, the seed is never written, and
/// the pair is one `btrfs check` finds clean.
#[test]
fn a_subvolume_handle_on_the_sprouted_pool_keeps_writing_with_the_seed() {
    let seed_path = std::path::PathBuf::from(temp_path!("seed-subvolume-seed.img"));
    let sprout_path = std::path::PathBuf::from(temp_path!("seed-subvolume-sprout.img"));
    for (name, copy) in [
        ("btrfs-seed.img", &seed_path),
        ("btrfs-sprout.img", &sprout_path),
    ] {
        std::fs::copy(fixture(&format!("seed/{name}")), copy).expect("copy the member");
    }
    let before = sha256_hex(&std::fs::read(&seed_path).expect("read the seed"));
    let seed = Arc::new(FileDevice::open_rw(&seed_path).unwrap()) as Arc<dyn BlockDevice>;
    let sprout = Arc::new(FileDevice::open_rw(&sprout_path).unwrap()) as Arc<dyn BlockDevice>;
    let mut fs = Filesystem::mount_pool_rw(vec![seed, sprout]).expect("mount the pair");
    let top = fs.lookup_path("/").unwrap().ino;
    let id = fs
        .create_subvolume(top, b"sub-on-sprout", 0o755, 0, 0)
        .expect("create a subvolume on the sprouted pool");
    let mut sub = fs.open_subvolume(id).expect("open the subvolume");
    drop(fs);
    let root = sub.lookup_path("/").unwrap().ino;
    let ino = sub
        .create(root, b"written-through-subvolume", 0o644, 0, 0)
        .expect("create a file through the subvolume handle");
    let first: Vec<u8> = (0..64 * 1024).map(|i| (i % 241) as u8).collect();
    assert_eq!(
        sub.write(ino, 0, &first)
            .expect("the first write after the create"),
        first.len()
    );
    let generation = sub.superblock().generation;
    let second: Vec<u8> = (0..64 * 1024).map(|i| (i % 239) as u8).collect();
    assert_eq!(
        sub.write(ino, first.len() as u64, &second)
            .expect("a second write through the handle handed back after a commit"),
        second.len()
    );
    assert!(
        sub.superblock().generation > generation,
        "nothing was committed"
    );
    let mut payload = first;
    payload.extend_from_slice(&second);
    assert_eq!(sub.read_file(ino).unwrap(), payload);
    drop(sub);
    assert_eq!(
        sha256_hex(&std::fs::read(&seed_path).unwrap()),
        before,
        "seed changed"
    );
    let out = oracle("bash")
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/check-pool.sh"))
        .arg(&sprout_path)
        .arg(&seed_path)
        .output();
    assert!(
        out.status.success(),
        "btrfs check of the pair: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let seed = Arc::new(FileDevice::open(&seed_path).unwrap()) as Arc<dyn BlockRead>;
    let sprout = Arc::new(FileDevice::open(&sprout_path).unwrap()) as Arc<dyn BlockRead>;
    let pool = Filesystem::mount_pool(vec![seed, sprout]).expect("reopen the pair");
    let reread = pool
        .open_subvolume(id)
        .expect("the subvolume through the pool")
        .read_path("/written-through-subvolume")
        .expect("the file through the pool");
    assert_eq!(reread, payload, "the file read back through the pool");
}

fn assert_on_sprout(fs: &Filesystem, at: u64, sprout_id: u64) {
    let chunk = fs.chunk_map().chunk_for(at).expect("a mapped allocation");
    assert!(
        chunk.stripes.iter().all(|s| s.devid == sprout_id),
        "allocation at {at} has a stripe on the seed"
    );
}
