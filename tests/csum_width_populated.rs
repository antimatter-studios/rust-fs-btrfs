//! Data checksums of every width, on a filesystem with files in it (#102).
//!
//! The four checksum geometries in the fixture matrix are empty
//! filesystems, and every populated fixture is crc32c, `mkfs.btrfs`'s
//! default. So the width the superblock names reached the data path only
//! as 4 bytes: passing a fixed 4 where the read passes the superblock's
//! digest length left every test green, and on a sha256 volume that
//! driver refuses every file past its first sector.
//!
//! These images need no mount: `mkfs.btrfs --rootdir` copies a directory
//! in and checksums its data with the algorithm `--csum` names. Each
//! image holds a file spanning many sectors, which must read back whole,
//! and must be refused once a byte of it is flipped underneath the
//! driver, which shows the digests were there and were compared.
//!
//! Skips without btrfs-progs, unless `BTRFS_ORACLE_FIXTURES=required`,
//! which the fixture CI job sets.

use fs_btrfs::superblock::ChecksumType;
use fs_btrfs::{Error, Filesystem};
use fs_core::{BlockRead, FileDevice};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Removes the scratch directory however the test ends.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 1 MiB that neither compresses nor repeats a sector, so every sector
/// has its own digest and a flip in one is caught by that one alone.
fn content() -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..1 << 20)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A populated image checksummed with `csum`, or `None` without
/// `mkfs.btrfs`.
fn image(csum: &str, data: &[u8]) -> Option<(Scratch, std::path::PathBuf)> {
    let dir = std::env::temp_dir().join(format!("btrfs-csum-{csum}-{}", std::process::id()));
    let scratch = Scratch(dir.clone());
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("data.bin"), data).unwrap();
    let img = dir.join("img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let made = match Command::new("mkfs.btrfs")
        .args(["-f", "--csum", csum, "--rootdir"])
        .arg(&root)
        .arg(&img)
        .output()
    {
        Ok(made) => made,
        Err(e) => {
            assert!(
                std::env::var("BTRFS_ORACLE_FIXTURES").as_deref() != Ok("required"),
                "BTRFS_ORACLE_FIXTURES=required, but mkfs.btrfs is not runnable: {e}"
            );
            return None;
        }
    };
    assert!(
        made.status.success(),
        "mkfs.btrfs --csum {csum}: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    Some((scratch, img))
}

/// Flips one byte of the first read holding `marker`, once armed.
struct Flip {
    inner: Arc<dyn BlockRead>,
    marker: Vec<u8>,
    flipped: AtomicBool,
}

impl BlockRead for Flip {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.inner.read_at(offset, buf)?;
        if let Some(at) = buf
            .windows(self.marker.len())
            .position(|w| w == self.marker)
        {
            buf[at] ^= 0xFF;
            self.flipped.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

#[test]
fn every_checksum_width_reads_a_populated_file_and_refuses_a_damaged_one() {
    let data = content();
    for (csum, kind) in [
        ("crc32c", ChecksumType::Crc32c),
        ("xxhash", ChecksumType::XxHash64),
        ("sha256", ChecksumType::Sha256),
        ("blake2", ChecksumType::Blake2b256),
    ] {
        let Some((_scratch, img)) = image(csum, &data) else {
            eprintln!("no mkfs.btrfs -- skipping");
            return;
        };
        let device: Arc<dyn BlockRead> = Arc::new(FileDevice::open(&img).unwrap());
        let fs = Filesystem::mount(device.clone()).unwrap();
        assert_eq!(
            fs.superblock().csum_type,
            kind,
            "{csum}: fixture checksum type"
        );
        assert_eq!(
            fs.read_path("/data.bin")
                .unwrap_or_else(|e| panic!("{csum}: a healthy file must read: {e:?}")),
            data,
            "{csum}: the file read back differs"
        );

        // A marker from the middle of the file: many sectors in, where a
        // digest read at the wrong width no longer lines up.
        let marker = data[600_000..600_032].to_vec();
        let flip = Arc::new(Flip {
            inner: device,
            marker,
            flipped: AtomicBool::new(false),
        });
        let damaged = Filesystem::mount(flip.clone() as Arc<dyn BlockRead>).unwrap();
        match damaged.read_path("/data.bin") {
            Err(Error::ChecksumMismatch { .. }) => {}
            other => panic!(
                "{csum}: a flipped byte must fail its checksum, got {:?}",
                other.map(|b| b.len())
            ),
        }
        assert!(
            flip.flipped.load(Ordering::SeqCst),
            "{csum}: the flip never landed"
        );
    }
}
