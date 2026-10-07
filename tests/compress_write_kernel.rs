//! Copy-on-write writes into files that ask for compression, judged by
//! what is not this crate (#265).
//!
//! A kernel-made volume holds a file the kernel wrote uncompressed and
//! then marked `chattr +c`, a file whose compression property names zstd,
//! and a file that asks for nothing. This crate overwrites each with
//! compressible bytes: the first is stored as a zlib extent, then
//! overwritten again — replacing the compressed extent it just wrote —
//! and the other two are stored as they are. `filefrag` asks the kernel
//! which extents are encoded; `btrfs check`, `btrfs check
//! --check-data-csum` (every on-disk sector against its digest), the
//! kernel's reads and a kernel scrub judge the rest.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, sha256_hex,
    temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A scratch volume, removed when it drops — including on a panic.
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
            "cd \"$MNT\"\n\
             head -c 65536 /dev/urandom > zlib.bin\n\
             head -c 65536 /dev/urandom > zstd.bin\n\
             head -c 65536 /dev/urandom > plain.bin\n\
             sync\n\
             chattr +c zlib.bin\n\
             btrfs property set zstd.bin compression zstd\n\
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

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open read-write"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write")
}

const FILES: [&str; 3] = ["zlib.bin", "zstd.bin", "plain.bin"];

#[test]
fn a_file_asking_for_compression_is_written_as_zlib_extents_the_kernel_reads() {
    let scratch = Scratch::new("compress-write");
    let image = scratch.image();

    let text: Vec<u8> = b"a line of very compressible text, ".repeat(2000);
    let text = &text[..65536];
    let mut fs = mount_rw(&image);
    let mut expected = Vec::new();
    for name in FILES {
        let ino = fs.lookup_path(&format!("/{name}")).unwrap().ino;
        fs.write(ino, 0, text)
            .unwrap_or_else(|e| panic!("writing {name}: {e}"));
        expected.push(text.to_vec());
    }
    // Again, into the zlib extent just written: a compressed extent is
    // replaced like any other.
    let ino = fs.lookup_path("/zlib.bin").unwrap().ino;
    fs.write(ino, 30_000, &[0x5a; 1000])
        .unwrap_or_else(|e| panic!("overwriting the compressed extent: {e}"));
    expected[0][30_000..31_000].copy_from_slice(&[0x5a; 1000]);
    for (name, want) in FILES.iter().zip(&expected) {
        let ino = fs.lookup_path(&format!("/{name}")).unwrap().ino;
        assert_eq!(
            &fs.read_file(ino).unwrap(),
            want,
            "{name} reads back differently through this driver"
        );
    }
    drop(fs);

    assert_btrfs_check_clean(&image, "after writes into files asking for compression");
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

    let seen = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "after the writes",
        "cd \"$MNT\"\n\
         for f in zlib.bin zstd.bin plain.bin; do \
           printf '%s %s %s\\n' \"$f\" \"$(sha256sum \"$f\" | cut -d' ' -f1)\" \
             \"$(filefrag -v \"$f\" | grep -c encoded || true)\"; done",
    );
    let lines: Vec<Vec<&str>> = seen.lines().map(|l| l.split(' ').collect()).collect();
    assert_eq!(lines.len(), 3, "the kernel reported:\n{seen}");
    for ((name, want), line) in FILES.iter().zip(&expected).zip(&lines) {
        assert_eq!(line[0], *name, "{seen}");
        assert_eq!(
            line[1],
            sha256_hex(want),
            "the kernel reads back different bytes from {name}:\n{seen}"
        );
    }
    assert_ne!(
        lines[0][2], "0",
        "zlib.bin asks for compression and has no encoded extent:\n{seen}"
    );
    for line in &lines[1..] {
        assert_eq!(
            line[2], "0",
            "{} was stored compressed, and nothing here writes zstd or was asked to:\n{seen}",
            line[0]
        );
    }

    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}
