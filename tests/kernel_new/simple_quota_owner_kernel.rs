//! A simple-quota volume whose data the kernel wrote, owner references and
//! all, is read and checked (#455, part of #270).
//!
//! With simple quotas on (`-O squota`), a kernel of 6.7 or later adds an
//! inline owner reference (`EXTENT_OWNER_REF`, type 172) to every data
//! extent item it writes, naming the subvolume charged for it. mkfs writes
//! none for the files `--rootdir` copies in, which is all
//! `tests/simple_quota_oracle.rs` has to read, so the reader and
//! `fsck.btrfs` had never met one the kernel made.
//!
//! THIS SUITE NEEDS THE DEBIAN 13 GUEST. Debian 12's kernel 6.1 cannot
//! mount a simple-quota volume at all. It is built only with the
//! `kernel-new-tests` feature, which `chore test:kernel-new` turns on while
//! pointing the harness at fs-linux-test-harness.debian-13.toml; every
//! other tier boots Debian 12 and never builds it. That tier holds it to a
//! floor, so a run that built nothing fails rather than passing empty.
//!
//! The volume is formatted by the guest's `mkfs.btrfs` and filled by its
//! kernel: files in the top-level subvolume, a reflinked copy sharing an
//! extent, and a subvolume of its own. The test then requires what the
//! issue's "done when" names before reading anything: the superblock
//! carries simple quotas, and `dump-tree -t extent` prints owner
//! references.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_super, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok,
    oracle, sha256_hex, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A scratch volume, removed when it drops -- including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-O", "squota"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs -O squota: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(
            &image.to_string_lossy(),
            name,
            "cd \"$MNT\"\n\
             mkdir dir\n\
             for i in 1 2 3; do echo \"owned file $i\" > dir/file-$i.txt; done\n\
             head -c 4194304 /dev/urandom > big.bin\n\
             head -c 200000 /dev/urandom > middle.bin\n\
             cp --reflink=always big.bin clone.bin\n\
             btrfs subvolume create \"$MNT/sv\" >/dev/null\n\
             head -c 1048576 /dev/urandom > sv/in-subvolume.bin\n\
             sync",
        );
        let scratch = Scratch(dir);
        let image = scratch.image();
        assert!(
            dump_super(&image).contains("SIMPLE_QUOTA"),
            "the superblock does not carry simple quotas:\n{}",
            dump_super(&image)
        );
        let extents = dump_tree(&image, "extent");
        let owners = extents.matches("extent owner root").count();
        // big.bin (shared by clone.bin), middle.bin and the subvolume's
        // file are at least one data extent each; the text files are
        // inline, in their leaves, and have none.
        assert!(
            owners >= 3,
            "the kernel wrote {owners} owner references; every data extent it wrote \
             under simple quotas carries one"
        );
        scratch
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mount(image: &Path) -> Filesystem {
    let dev = FileDevice::open(image).expect("open");
    Filesystem::mount(Arc::new(dev)).expect("a simple-quota volume mounts read-only")
}

#[test]
fn every_file_reads_what_the_kernel_wrote() {
    let scratch = Scratch::new("squota-owner-read");
    let image = scratch.image();
    let seen = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "the files",
        "cd \"$MNT\"\n\
         find . -type f | sort | while read -r p; do \
           printf '%s %s %s\\n' \"${p#.}\" \"$(stat -c%s \"$p\")\" \
             \"$(sha256sum \"$p\" | cut -d' ' -f1)\"; done",
    );
    let fs = mount(&image);
    let mut checked = 0;
    for line in seen.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        assert_eq!(f.len(), 3, "a line the kernel printed: {line:?}");
        let got = fs.read_path(f[0]).unwrap_or_else(|e| panic!("{}: {e}", f[0]));
        assert_eq!(got.len().to_string(), f[1], "{}: length", f[0]);
        assert_eq!(sha256_hex(&got), f[2], "{}: contents", f[0]);
        checked += 1;
    }
    assert_eq!(checked, 7, "the kernel listed:\n{seen}");
}

#[test]
fn fsck_finds_the_volume_clean_as_btrfs_check_does() {
    let scratch = Scratch::new("squota-owner-fsck");
    let image = scratch.image();
    assert_btrfs_check_clean(&image, "as the kernel left it");
    let report = fs_btrfs::check::check(&mount(&image));
    let findings: Vec<String> = report.findings.iter().map(|f| f.what.clone()).collect();
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn a_read_write_mount_is_refused_naming_simple_quotas() {
    let scratch = Scratch::new("squota-owner-rw");
    let rw = FileDevice::open_rw(scratch.image()).expect("open read-write");
    match Filesystem::mount_rw(Arc::new(rw) as Arc<dyn BlockDevice>) {
        Ok(_) => panic!("a simple-quota volume must not mount for writing"),
        Err(e) => assert!(
            e.to_string().contains("simple quota"),
            "the refusal must name simple quotas: {e}"
        ),
    }
}
