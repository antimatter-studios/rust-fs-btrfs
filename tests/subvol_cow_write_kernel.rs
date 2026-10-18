//! A copy-on-write write into a file inside a subvolume other than the
//! top level, judged by btrfs-progs and the kernel (#445, part of #261).
//!
//! The guest kernel makes a volume holding a subvolume `sv` with a
//! checksummed file in it and a file at the top. This crate opens `sv` on
//! a writable mount and writes into the middle of that file, one
//! transaction planned against the subvolume's own tree. Then:
//!
//! - `btrfs check --readonly --check-data-csum` must find the volume
//!   clean, and a kernel scrub must pass;
//! - the kernel, mounting `sv` with `-o subvol=`, must read back exactly
//!   the bytes written over the bytes it left;
//! - the kernel must read the top-level subvolume's file unchanged.

use std::sync::Arc;

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};

/// Where the write starts, and how long it is: past the first sector
/// and across the next ones, so it splits the file's extent.
const AT: u64 = 4096;
const LEN: usize = 5000;

/// What the kernel writes before this crate touches the volume: 64 KiB
/// of a repeating pattern in `sv/data`.
const FILL: &str = "btrfs subvolume create \"$MNT/sv\" >/dev/null\n\
     printf 'abcdefgh%.0s' $(seq 8192) > \"$MNT/sv/data\"\n\
     printf 'held at top\\n' > \"$MNT/top-file\"\n\
     sync";

/// The kernel's view of `sv`, mounted on its own, next to the bytes the
/// file must hold: the pattern with `LEN` bytes of `W` at `AT`.
const READ: &str = "src=$(findmnt -no SOURCE \"$MNT\")\n\
     mkdir -p /tmp/sv-only\n\
     mount -o ro,subvol=sv \"$src\" /tmp/sv-only\n\
     echo file:; sha256sum < /tmp/sv-only/data\n\
     umount /tmp/sv-only\n\
     printf 'abcdefgh%.0s' $(seq 8192) > /tmp/pattern\n\
     echo want:; { head -c 4096 /tmp/pattern; head -c 5000 /dev/zero | tr '\\0' W; \
     tail -c +9097 /tmp/pattern; } | sha256sum\n\
     echo top:; cat \"$MNT/top-file\"";

#[test]
fn a_write_inside_a_subvolume_is_clean_and_the_kernel_reads_it_back() {
    let dir = std::path::PathBuf::from(temp_path!("subvol-cow-write-kernel"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .expect("size the image");
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384"])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "mkfs.btrfs: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    let img = image.to_string_lossy().into_owned();
    guest_kernel_write_ok(&img, "subvol-cow-write-fill", FILL);

    {
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open rw"));
        let top = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
        let id = top.subvolume_at(b"/sv").expect("find sv");
        let mut fs = top.open_subvolume(id).expect("open sv");
        let ino = fs.lookup_path("/data").expect("sv's file").ino;
        let written = fs.write(ino, AT, &[b'W'; LEN]).expect("write inside sv");
        assert_eq!(written, LEN, "the whole write lands");
        assert_eq!(
            fs.subvolume_at(b"/").expect("the mount's own subvolume"),
            id,
            "after its commit the mount is still on sv"
        );
        let mut back = vec![0; LEN];
        fs.read_at(ino, AT, &mut back).expect("read back inside sv");
        assert!(
            back.iter().all(|&b| b == b'W'),
            "the handle reads its write"
        );
    }

    assert_btrfs_check_clean(&image, "subvol-cow-write");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let seen = guest_kernel_read_ok(&img, "subvol-cow-write-read", READ);
    let lines: Vec<&str> = seen.lines().collect();
    let hash = |label: &str| {
        let at = lines
            .iter()
            .position(|l| *l == label)
            .unwrap_or_else(|| panic!("no {label} in the kernel's output:\n{seen}"));
        lines[at + 1].split_whitespace().next().unwrap_or_default()
    };
    assert_eq!(
        hash("file:"),
        hash("want:"),
        "the kernel reads other bytes in sv/data than were written:\n{seen}"
    );
    assert!(
        seen.ends_with("top:\nheld at top\n"),
        "the top level's file changed:\n{seen}"
    );
    guest_kernel_write_ok(
        &img,
        "subvol-cow-write-scrub",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let _ = std::fs::remove_dir_all(&dir);
}
