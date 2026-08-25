//! The filesystem layer, read against filesystems the Linux kernel made.
//!
//! Everything below the `Filesystem` handle — superblock, chunk map,
//! B-tree — is already cross-validated against the reference tooling.
//! This file checks the layer above: that the driver resolves paths,
//! lists directories and returns file contents that match what the
//! kernel actually wrote.
//!
//! The `deep4k` and `deep16k` fixtures are built by mounting a real
//! Btrfs filesystem and writing 20,000 and 60,000 files into `/many/`,
//! each named `f<N>.txt` and containing the decimal `<N>` and a newline.
//! That gives an exactly known expected value for every file without
//! shipping a manifest, and it is enough files to push the fs tree to
//! level 2, so reads go through real multi-level descent.
//!
//! Fixtures are gitignored, so these skip cleanly on a fresh clone.

use fs_btrfs::Filesystem;
use fs_core::FileDevice;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixtures() -> Vec<(String, PathBuf)> {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let Ok(entries) = std::fs::read_dir(&share) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("img"))
        .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), p))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn mount(img: &Path, label: &str) -> Filesystem {
    let dev = FileDevice::open(img).unwrap_or_else(|e| panic!("{label}: open: {e}"));
    Filesystem::mount(Arc::new(dev)).unwrap_or_else(|e| panic!("{label}: mount: {e}"))
}

/// Every fixture must mount and expose a root directory.
#[test]
fn every_fixture_mounts_and_has_a_root() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!("no fixtures in .vm-share — skipping");
        return;
    }
    for (label, img) in &fixtures {
        let fs = mount(img, label);
        let root = fs
            .root_inode()
            .unwrap_or_else(|e| panic!("{label}: root inode: {e}"));
        assert!(root.is_dir(), "{label}: the root inode is not a directory");

        // Listing the root must succeed even when it is empty.
        let entries = fs
            .read_dir(root.ino)
            .unwrap_or_else(|e| panic!("{label}: listing the root: {e}"));
        assert!(
            entries.iter().all(|e| e.name != b"." && e.name != b".."),
            "{label}: `.` or `..` leaked into the listing"
        );
        eprintln!("  {label}: root has {} entries", entries.len());
    }
}

/// The populated fixtures let every file's contents be predicted
/// exactly, so this is a content check rather than a smoke test.
#[test]
fn reads_back_the_files_the_kernel_wrote() {
    let deep: Vec<_> = fixtures()
        .into_iter()
        .filter(|(name, _)| name.contains("deep"))
        .collect();
    if deep.is_empty() {
        eprintln!("no populated fixtures — skipping");
        return;
    }

    for (label, img) in &deep {
        let fs = mount(img, label);

        let many = fs
            .lookup_path("/many")
            .unwrap_or_else(|e| panic!("{label}: /many should exist: {e}"));
        assert!(many.is_dir(), "{label}: /many is not a directory");

        let entries = fs
            .read_dir(many.ino)
            .unwrap_or_else(|e| panic!("{label}: listing /many: {e}"));
        let expected = if label.contains("4k") { 20_000 } else { 60_000 };
        assert_eq!(
            entries.len(),
            expected,
            "{label}: /many should hold {expected} entries"
        );

        // Sample across the whole range rather than the first few, so
        // the reads exercise different subtrees.
        let step = expected / 40;
        let mut checked = 0usize;
        for n in (1..=expected).step_by(step) {
            let path = format!("/many/f{n}.txt");
            let data = fs
                .read_path(&path)
                .unwrap_or_else(|e| panic!("{label}: reading {path}: {e}"));
            let want = format!("{n}\n");
            assert_eq!(
                String::from_utf8_lossy(&data),
                want,
                "{label}: {path} contents differ from what the kernel wrote"
            );
            checked += 1;
        }
        assert!(checked >= 20, "{label}: only {checked} files sampled");
        eprintln!(
            "  {label}: {} entries, {checked} files byte-exact",
            entries.len()
        );
    }
}

/// Names in the listing must resolve, and resolve to the inode the
/// listing named. A listing that reports entries a lookup cannot find is
/// worse than an empty one.
#[test]
fn every_listed_name_resolves_to_the_inode_it_named() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }
    for (label, img) in &fixtures {
        let fs = mount(img, label);
        let root = fs.root_inode().expect("root");
        for e in fs.read_dir(root.ino).expect("listing") {
            let name = String::from_utf8_lossy(&e.name).into_owned();
            let found = fs
                .lookup(root.ino, &e.name)
                .unwrap_or_else(|err| panic!("{label}: `{name}` was listed but not found: {err}"));
            assert_eq!(
                found.ino, e.ino,
                "{label}: `{name}` resolved to a different inode than the listing gave"
            );
        }
    }
}

/// Refusals, which are half of what a filesystem driver is for. A driver
/// that returns something for a case it does not understand hands a user
/// silently wrong data with no way to detect it.
#[test]
fn refuses_what_it_cannot_answer() {
    let deep: Vec<_> = fixtures()
        .into_iter()
        .filter(|(name, _)| name.contains("deep"))
        .collect();
    let Some((label, img)) = deep.first() else {
        eprintln!("no populated fixture — skipping");
        return;
    };
    let fs = mount(img, label);

    assert!(
        matches!(
            fs.lookup_path("/definitely-absent"),
            Err(fs_btrfs::Error::NotFound)
        ),
        "a missing path must be NotFound"
    );
    assert!(
        matches!(
            fs.lookup_path("/many/f1.txt/child"),
            Err(fs_btrfs::Error::NotADirectory)
        ),
        "descending through a file must be NotADirectory"
    );
    assert!(
        matches!(
            fs.lookup_path("/many/../many"),
            Err(fs_btrfs::Error::UnsupportedFeature(_))
        ),
        "`..` must be declined rather than silently resolved"
    );

    // Reading a directory's bytes as file contents is not meaningful.
    let many = fs.lookup_path("/many").expect("/many");
    assert!(
        matches!(fs.read_file(many.ino), Err(fs_btrfs::Error::NotAFile)),
        "reading a directory as a file must be refused"
    );

    // A read starting past end of file is empty, not an error.
    let f = fs.lookup_path("/many/f1.txt").expect("f1");
    let mut buf = [0u8; 16];
    assert_eq!(fs.read_at(f.ino, f.size + 100, &mut buf).expect("read"), 0);
}

/// Redundant separators and `.` components are ordinary, and the root is
/// reachable by each of its spellings.
#[test]
fn path_spellings_are_tolerated() {
    let deep: Vec<_> = fixtures()
        .into_iter()
        .filter(|(name, _)| name.contains("deep"))
        .collect();
    let Some((label, img)) = deep.first() else {
        eprintln!("no populated fixture — skipping");
        return;
    };
    let fs = mount(img, label);

    let direct = fs.lookup_path("/many/f1.txt").expect("direct");
    for messy in ["//many//f1.txt", "/./many/./f1.txt", "many/f1.txt"] {
        let got = fs
            .lookup_path(messy)
            .unwrap_or_else(|e| panic!("{label}: `{messy}` should resolve: {e}"));
        assert_eq!(got.ino, direct.ino, "`{messy}` resolved elsewhere");
    }

    let root = fs.root_inode().expect("root").ino;
    for spelling in ["/", "", ".", "/./"] {
        assert_eq!(
            fs.lookup_path(spelling).expect("root resolves").ino,
            root,
            "`{spelling}` did not resolve to the root"
        );
    }
}
