//! `fs_btrfs_write_file` on an ordinary copy-on-write file, judged by what
//! is not this crate (#274).
//!
//! The C ABI is how a host application writes, so it must reach the same
//! copy-on-write path the library has: a write into a file that is not
//! `nodatacow` lands in new extents and is committed as one transaction.
//! The volume is made by `mkfs.btrfs` and filled by the kernel in the
//! harness VM; after the ABI writes it, `btrfs check` inspects the whole
//! volume and the kernel mounts it and reads every byte back, and
//! `filefrag` says whether the extent moved.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, sha256_hex,
    temp_path,
};
use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

/// A scratch volume, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    /// A fresh 256 MiB volume from `mkfs.btrfs`: a checksummed file, then,
    /// on a `nodatasum` remount, an ordinary copy-on-write file in two
    /// extents that ends partway through a sector.
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "16384"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(
            &image.to_string_lossy(),
            name,
            "head -c 65536 /dev/urandom > \"$MNT/summed.bin\"\n\
             sync\n\
             mount -o remount,nodatasum \"$MNT\"\n\
             head -c 65536 /dev/urandom > \"$MNT/cow.bin\"\n\
             sync\n\
             head -c 40000 /dev/urandom >> \"$MNT/cow.bin\"\n\
             sync",
        );
        Scratch(dir)
    }

    fn image(&self) -> String {
        self.0.join("fs.img").to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Each file's SHA-256 and the physical address of its first extent.
fn kernel_view(image: &str, what: &str) -> Vec<(String, String)> {
    let mut script = String::new();
    for name in ["cow.bin", "summed.bin"] {
        script.push_str(&format!(
            "printf '%s %s\\n' \"$(sha256sum \"$MNT/{name}\" | cut -d' ' -f1)\" \
             \"$(filefrag -v \"$MNT/{name}\" | awk '$1 == \"0:\" {{print $4; exit}}')\"\n"
        ));
    }
    guest_kernel_read_ok(image, what, &script)
        .lines()
        .map(|line| {
            let mut words = line.split_whitespace();
            (
                words.next().unwrap_or_default().to_string(),
                words.next().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Two writes through one handle — inside the first extent, then across
/// the boundary into the second — each read back through the ABI, then
/// judged by `btrfs check` and the kernel.
#[test]
fn the_c_abi_writes_a_copy_on_write_file_the_checker_and_kernel_accept() {
    let scratch = Scratch::new("capi-cow-write");
    let image = scratch.image();
    let before = kernel_view(&image, "before the ABI write");

    let device = CString::new(image.as_str()).unwrap();
    let fs = unsafe { fs_btrfs_mount_rw(device.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw failed: {}", last_error());
    let path = CString::new("/cow.bin").unwrap();

    let mut expected = vec![0u8; 105_536];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            path.as_ptr(),
            expected.as_mut_ptr().cast::<c_void>(),
            0,
            expected.len() as u64,
        )
    };
    assert_eq!(n, expected.len() as i64, "{}", last_error());

    let writes: [(u64, Vec<u8>); 2] = [
        (5000, b"through the C ABI, copy-on-write ".repeat(3)),
        (61440, vec![0xa5; 8192]),
    ];
    for (offset, bytes) in &writes {
        let n = unsafe {
            fs_btrfs_write_file(
                fs,
                path.as_ptr(),
                bytes.as_ptr().cast::<c_void>(),
                *offset,
                bytes.len() as u64,
            )
        };
        assert_eq!(
            n,
            bytes.len() as i64,
            "writing {} bytes at {offset}: {}",
            bytes.len(),
            last_error()
        );
        expected[*offset as usize..*offset as usize + bytes.len()].copy_from_slice(bytes);

        let mut back = vec![0u8; expected.len()];
        let r = unsafe {
            fs_btrfs_read_file(
                fs,
                path.as_ptr(),
                back.as_mut_ptr().cast::<c_void>(),
                0,
                back.len() as u64,
            )
        };
        assert_eq!(r, back.len() as i64, "{}", last_error());
        assert!(
            back == expected,
            "the ABI reads back different bytes after the write at {offset}"
        );
    }
    unsafe { fs_btrfs_umount(fs) };

    assert_btrfs_check_clean(std::path::Path::new(&image), "after the ABI's writes");
    let after = kernel_view(&image, "after the ABI write");
    assert_eq!(
        after[0].0,
        sha256_hex(&expected),
        "the kernel reads back different bytes than the ABI wrote"
    );
    assert_ne!(
        after[0].1, before[0].1,
        "cow.bin's first extent is where it was, so the write was not copy-on-write"
    );
    assert_eq!(
        after[1], before[1],
        "summed.bin changed under the ABI write"
    );
}
