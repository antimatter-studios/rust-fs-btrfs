//! A volume with simple quotas is read, and refused for writing (#270).
//!
//! Simple quotas (`-O squota`, the `simple_quota` incompat bit) add an
//! owner reference to each data extent item and keep usage in the quota
//! tree. Neither changes where a file's bytes are, so reading is sound,
//! but a write that allocated an extent without its owner reference or
//! the usage that goes with it would leave the accounting wrong, so a
//! read-write mount is refused by name.
//!
//! The fixture is `squota/btrfs-squota.img`, formatted with simple quotas
//! on by btrfs-progs' own `mkfs.btrfs --rootdir` from a directory tree in
//! the harness VM (the guest's kernel predates simple quotas, so it
//! cannot fill one); the builder refuses to publish it unless the
//! superblock carries the bit and that btrfs-progs' `btrfs check` finds it
//! clean. Its manifest is every file's size and SHA-256 as they were in
//! the directory.
//!
//! WHAT THIS DOES NOT COVER: an extent with an owner reference. mkfs
//! writes none for the files it copies in, as the kernel writes none for
//! an extent older than the feature's enabling; only a kernel of 6.7 or
//! later, writing under simple quotas, does. Reading a file never looks
//! at the extent tree, but `fsck.btrfs` does, and its handling of that
//! inline reference waits for a guest that can make one.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{fixture, sha256_hex, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::sync::Arc;

fn image() -> std::path::PathBuf {
    fixture("squota/btrfs-squota.img")
}

fn mount() -> Filesystem {
    let dev = FileDevice::open(image()).expect("open squota/btrfs-squota.img");
    Filesystem::mount(Arc::new(dev)).expect("a simple-quota volume mounts read-only")
}

#[test]
fn every_file_reads_what_mkfs_copied_in() {
    let fs = mount();
    let manifest =
        std::fs::read_to_string(fixture("squota/btrfs-squota.manifest")).expect("manifest");
    let mut checked = 0;
    for line in manifest.lines().take_while(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 3, "a manifest line: {line:?}");
        let got = fs
            .read_path(f[0])
            .unwrap_or_else(|e| panic!("{}: {e}", f[0]));
        assert_eq!(got.len().to_string(), f[1], "{}: length", f[0]);
        assert_eq!(sha256_hex(&got), f[2], "{}: contents", f[0]);
        checked += 1;
    }
    assert!(checked >= 5, "the manifest names {checked} files");
}

#[test]
fn fsck_finds_the_volume_clean_as_btrfs_check_does() {
    let report = fs_btrfs::check::check(&mount());
    let findings: Vec<String> = report.findings.iter().map(|f| f.what.clone()).collect();
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn a_read_write_mount_is_refused_naming_simple_quotas() {
    let dir = std::path::PathBuf::from(temp_path!("squota-rw"));
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("img");
    std::fs::copy(image(), &copy).expect("copy the fixture");
    let rw = FileDevice::open_rw(&copy).expect("open the copy for writing");
    match Filesystem::mount_rw(Arc::new(rw) as Arc<dyn BlockDevice>) {
        Ok(_) => panic!("a simple-quota volume must not mount for writing"),
        Err(e) => assert!(
            e.to_string().contains("simple quota"),
            "the refusal must name simple quotas: {e}"
        ),
    }
}
