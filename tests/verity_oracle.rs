//! A volume with fs-verity files reads, every block of a sealed file is
//! checked against its Merkle tree, and what the driver reports for each
//! file is what the kernel reports (#270).
//!
//! The fixture is `features/btrfs-verity.img`, made by `mkfs.btrfs` and
//! filled and sealed by the kernel (`fsverity enable`) in the harness VM.
//! Its manifest is one line per file: path, size and SHA-256 as the kernel
//! read them, then for a sealed file `fsverity measure` and the SHA-256 of
//! the Merkle tree and of the descriptor as `fsverity dump_metadata` reads
//! them back, and `-` for a file that is not sealed. The builder refuses to
//! publish it unless the superblock carries the verity bit, the kernel
//! measures all six sealed files and `btrfs check` finds it clean.
//!
//! `features/btrfs-verity-tampered.img` is a copy with 64 bytes of
//! `/nodatasum/tamper.bin`'s second block overwritten. That file has no
//! data checksums, so fs-verity is the only thing that can notice, and the
//! builder publishes the copy only once the kernel has read the file's
//! first block from it and refused the file whole. That is the behaviour
//! held here: the untouched block reads, the file does not.

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{fixture, sha256_hex, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::sync::Arc;

const TAMPERED: &str = "/nodatasum/tamper.bin";

fn mount(name: &str) -> Filesystem {
    let dev = FileDevice::open(fixture(name)).unwrap_or_else(|e| panic!("open {name}: {e}"));
    Filesystem::mount(Arc::new(dev)).unwrap_or_else(|e| panic!("mount {name}: {e}"))
}

/// One manifest line.
struct Entry {
    path: String,
    size: String,
    sha256: String,
    /// `None` for a file the kernel does not measure: one not sealed.
    sealed: Option<Sealed>,
}

struct Sealed {
    measurement: String,
    tree_sha256: String,
    descriptor_sha256: String,
}

fn manifest() -> Vec<Entry> {
    let text = std::fs::read_to_string(fixture("features/btrfs-verity.manifest"))
        .expect("features/btrfs-verity.manifest");
    let entries: Vec<Entry> = text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 6, "a manifest line: {line:?}");
            Entry {
                path: f[0].to_string(),
                size: f[1].to_string(),
                sha256: f[2].to_string(),
                sealed: (f[3] != "-").then(|| Sealed {
                    measurement: f[3].to_string(),
                    tree_sha256: f[4].to_string(),
                    descriptor_sha256: f[5].to_string(),
                }),
            }
        })
        .collect();
    let sealed = entries.iter().filter(|e| e.sealed.is_some()).count();
    assert_eq!(sealed, 6, "the manifest names {sealed} sealed files");
    assert!(
        entries.iter().any(|e| e.sealed.is_none()),
        "the manifest names no file without verity"
    );
    entries
}

#[test]
fn every_file_reads_what_the_kernel_wrote() {
    let fs = mount("features/btrfs-verity.img");
    for e in manifest() {
        let got = fs
            .read_path(&e.path)
            .unwrap_or_else(|err| panic!("{}: {err}", e.path));
        assert_eq!(got.len().to_string(), e.size, "{}: length", e.path);
        assert_eq!(sha256_hex(&got), e.sha256, "{}: contents", e.path);
    }
}

#[test]
fn each_sealed_file_measures_as_the_kernel_measures_it() {
    let fs = mount("features/btrfs-verity.img");
    for e in manifest() {
        let ino = fs
            .lookup_path(&e.path)
            .unwrap_or_else(|err| panic!("{}: {err}", e.path))
            .ino;
        let verity = fs
            .verity(ino)
            .unwrap_or_else(|err| panic!("{}: {err}", e.path));
        let Some(want) = e.sealed else {
            assert!(verity.is_none(), "{} is not sealed: {verity:?}", e.path);
            continue;
        };
        let v = verity.unwrap_or_else(|| panic!("{} is sealed, and no verity was found", e.path));
        assert_eq!(v.measurement(), want.measurement, "{}: digest", e.path);
        assert_eq!(
            sha256_hex(&v.descriptor),
            want.descriptor_sha256,
            "{}: the descriptor as stored",
            e.path
        );
        let tree = fs
            .verity_merkle_tree(ino)
            .unwrap_or_else(|err| panic!("{}: {err}", e.path));
        assert_eq!(
            sha256_hex(&tree),
            want.tree_sha256,
            "{}: the Merkle tree as stored ({} bytes)",
            e.path,
            tree.len()
        );
    }
}

#[test]
fn a_sealed_file_reads_in_pieces_that_straddle_its_blocks() {
    let fs = mount("features/btrfs-verity.img");
    for path in [
        "/two-levels.bin",
        "/sha512-salted.bin",
        "/partial-block.bin",
    ] {
        let ino = fs.lookup_path(path).expect(path).ino;
        let whole = fs.read_file(ino).expect(path);
        let mut at = 0usize;
        // 5000 bytes at a time: no piece starts or ends on a block.
        while at < whole.len() {
            let mut buf = vec![0u8; 5000];
            let n = fs.read_at(ino, at as u64, &mut buf).expect(path);
            assert_eq!(&buf[..n], &whole[at..at + n], "{path} at {at}");
            at += n;
        }
    }
}

#[test]
fn fsck_finds_the_volume_clean_as_btrfs_check_does() {
    let report = fs_btrfs::check::check(&mount("features/btrfs-verity.img"));
    let findings: Vec<String> = report.findings.iter().map(|f| f.what.clone()).collect();
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn a_tampered_block_is_refused_as_the_kernel_refuses_it() {
    let good = mount("features/btrfs-verity.img");
    let bad = mount("features/btrfs-verity-tampered.img");
    let ino = bad.lookup_path(TAMPERED).expect(TAMPERED).ino;

    // The first block is untouched, and reads, as the kernel read it.
    let mut first = vec![0u8; 4096];
    assert_eq!(bad.read_at(ino, 0, &mut first).expect("block 0"), 4096);
    let mut want = vec![0u8; 4096];
    good.read_at(ino, 0, &mut want)
        .expect("block 0 of the original");
    assert_eq!(first, want, "block 0");

    // The file whole is refused, naming the block that does not verify.
    match bad.read_path(TAMPERED) {
        Err(Error::ChecksumMismatch { offset, .. }) => assert_eq!(offset, 4096),
        other => panic!("{TAMPERED} must be refused, not {other:?}"),
    }
    // As is any read that touches the second block.
    let mut piece = vec![0u8; 10];
    assert!(bad.read_at(ino, 4096 + 4000, &mut piece).is_err());

    // Every other file is untouched and reads as it did.
    for e in manifest().into_iter().filter(|e| e.path != TAMPERED) {
        let got = bad
            .read_path(&e.path)
            .unwrap_or_else(|err| panic!("{}: {err}", e.path));
        assert_eq!(sha256_hex(&got), e.sha256, "{}: contents", e.path);
    }
}

#[test]
fn a_read_write_mount_is_refused_naming_fs_verity() {
    let dir = std::path::PathBuf::from(temp_path!("verity-rw"));
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("img");
    std::fs::copy(fixture("features/btrfs-verity.img"), &copy).expect("copy the fixture");
    let rw = FileDevice::open_rw(&copy).expect("open the copy for writing");
    match Filesystem::mount_rw(Arc::new(rw) as Arc<dyn BlockDevice>) {
        Ok(_) => panic!("a volume with fs-verity files must not mount for writing"),
        Err(e) => assert!(
            e.to_string().contains("fs-verity"),
            "the refusal must name fs-verity: {e}"
        ),
    }
}
