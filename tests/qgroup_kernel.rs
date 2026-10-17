//! A volume with quotas enabled -- the quota groups `btrfs quota enable`
//! turns on, not simple quotas -- is read, and refused for writing (#270).
//!
//! Quota groups keep, per subvolume and per higher-level group, the bytes
//! it references and the bytes only it references, in the quota tree
//! (objectid 8). The kernel updates them in the transaction that moves an
//! extent. This driver does not: a write that allocated or released an
//! extent would leave every count it touched wrong, and `btrfs check`
//! reports such a volume as damaged. So a read-write mount is refused by
//! name, as it is for simple quotas, and a read-only one is not affected.
//!
//! The volume is formatted in the harness VM and filled by the kernel: a
//! subvolume, a snapshot of it, a level-1 group holding the subvolume,
//! limits on both, and a rescan so every count is the kernel's own.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, sha256_hex,
    temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::PathBuf;
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
        let made = oracle("mkfs.btrfs").args(["-q", "-f"]).arg(&image).output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(
            &image.to_string_lossy(),
            name,
            "cd \"$MNT\"\n\
             btrfs quota enable \"$MNT\"\n\
             btrfs subvolume create \"$MNT/sv\" >/dev/null\n\
             head -c 1048576 /dev/urandom > sv/a.bin\n\
             echo 'in the subvolume' > sv/small.txt\n\
             head -c 300000 /dev/urandom > top.bin\n\
             btrfs subvolume snapshot \"$MNT/sv\" \"$MNT/snap\" >/dev/null\n\
             head -c 65536 /dev/urandom > snap/own.bin\n\
             btrfs qgroup create 1/100 \"$MNT\"\n\
             btrfs qgroup assign --no-rescan 0/256 1/100 \"$MNT\"\n\
             btrfs qgroup limit 50M 0/256 \"$MNT\"\n\
             btrfs qgroup limit -e 20M 1/100 \"$MNT\"\n\
             sync\n\
             btrfs quota rescan -w \"$MNT\" >/dev/null\n\
             sync",
        );
        Scratch(dir)
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

#[test]
fn every_file_reads_what_the_kernel_wrote() {
    let scratch = Scratch::new("qgroup-read");
    let image = scratch.image();
    assert_btrfs_check_clean(&image, "as the kernel left it");
    let seen = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "the files",
        "cd \"$MNT\"\n\
         find . -type f | sort | while read -r p; do \
           printf '%s %s\\n' \"${p#.}\" \"$(sha256sum \"$p\" | cut -d' ' -f1)\"; done",
    );
    let dev = FileDevice::open(&image).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("a volume with quotas mounts read-only");
    let mut checked = 0;
    for line in seen.lines() {
        let (path, sha) = line.split_once(' ').expect("path and digest");
        let got = fs.read_path(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(sha256_hex(&got), sha, "{path}");
        checked += 1;
    }
    assert_eq!(checked, 6, "the kernel listed:\n{seen}");
}

#[test]
fn a_read_write_mount_is_refused_naming_quotas() {
    let scratch = Scratch::new("qgroup-rw");
    let rw = FileDevice::open_rw(scratch.image()).expect("open read-write");
    match Filesystem::mount_rw(Arc::new(rw) as Arc<dyn BlockDevice>) {
        Ok(_) => panic!(
            "a volume with quotas enabled must not mount for writing: a write would leave \
             its quota groups' counts wrong"
        ),
        Err(e) => assert!(
            e.to_string().contains("quota"),
            "the refusal must name quotas: {e}"
        ),
    }
}
