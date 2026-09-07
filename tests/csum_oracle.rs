//! Data checksums, against a filesystem the Linux kernel made.
//!
//! The kernel checksums every sector of an ordinary data extent and
//! verifies it on the way out; a file with a flipped byte comes back as
//! `EIO`, naming the file. This driver used to copy the extent's bytes
//! out with no check at all, so the same read returned the damaged
//! bytes and reported success.
//!
//! ## How the damage is applied
//!
//! Not by writing to the fixture. The image is wrapped in a `BlockRead`
//! that flips one byte of whichever read starts with the file's first
//! thirty-two bytes — a marker taken from the file's own random
//! content, so nothing else on the disk can match it.
//!
//! That keeps the fixture untouched, which matters because it is shared
//! with every other oracle here, and it means the test does not have to
//! know where on the disk the extent landed. The damage arrives
//! underneath the driver exactly as bit-rot would.
//!
//! ## Why this fixture
//!
//! `btrfs-nodatacow.img` carries both halves of the question. `/cow.bin`
//! is an ordinary file, so every sector of it has a digest in the csum
//! tree. `/nc/inplace.bin` was written into a `chattr +C` directory, so
//! it is `NODATACOW|NODATASUM` and has none. The same flip has to be
//! refused on one and served on the other: a driver that failed both
//! would refuse files the kernel reads happily, and one that served
//! both is the defect.
//!
//! Fixtures are gitignored, so this skips cleanly on a fresh clone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use fs_btrfs::{Error, Filesystem};
use fs_core::{BlockRead, FileDevice};

mod common;

/// The fixture with a checksummed file and a `NODATASUM` one.
fn nodatacow_image() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".vm-share")
        .join("btrfs-nodatacow.img");
    p.exists().then_some(p)
}

/// A device that flips the first byte of the sector holding `marker`.
///
/// `armed` is what makes the same wrapper serve as its own control: the
/// disarmed pass proves the reads that follow differ because of the
/// flip and not because of the wrapper.
struct FlipsTheSectorHolding {
    inner: Arc<dyn BlockRead>,
    marker: Vec<u8>,
    armed: bool,
    flipped: AtomicBool,
}

impl FlipsTheSectorHolding {
    fn new(inner: Arc<dyn BlockRead>, marker: &[u8], armed: bool) -> Arc<Self> {
        Arc::new(FlipsTheSectorHolding {
            inner,
            marker: marker.to_vec(),
            armed,
            flipped: AtomicBool::new(false),
        })
    }

    /// Whether the flip actually landed.
    ///
    /// Asserted by every test that expects damage. A wrapper that never
    /// matched would make a refusal test pass for the wrong reason and
    /// a "still reads" test pass vacuously.
    fn flipped(&self) -> bool {
        self.flipped.load(Ordering::SeqCst)
    }
}

impl BlockRead for FlipsTheSectorHolding {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.inner.read_at(offset, buf)?;
        if self.armed
            && buf.len() >= self.marker.len()
            && buf[..self.marker.len()] == self.marker[..]
        {
            buf[0] ^= 0xFF;
            self.flipped.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

/// Read a file by path off the fixture, undamaged.
fn read_clean(image: &Path, path: &str) -> Vec<u8> {
    let dev: Arc<dyn BlockRead> = Arc::new(FileDevice::open(image).expect("open fixture"));
    let fs = Filesystem::mount(dev).expect("mount fixture");
    let inode = fs.lookup_path(path).expect("look up the file");
    fs.read_file(inode.ino).expect("read the file")
}

/// Mount the fixture through the flipping wrapper.
fn mount_flipping(
    image: &Path,
    marker: &[u8],
    armed: bool,
) -> (Filesystem, Arc<FlipsTheSectorHolding>) {
    let file: Arc<dyn BlockRead> = Arc::new(FileDevice::open(image).expect("open fixture"));
    let dev = FlipsTheSectorHolding::new(file, marker, armed);
    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount fixture");
    (fs, dev)
}

/// A flipped byte in a checksummed file is refused rather than returned.
///
/// This is the failure the crate's own module doc says it exists to
/// prevent: "returning plausible-but-wrong file contents is the one
/// failure a caller cannot detect". Before the csum tree was read, this
/// read returned the flipped byte and `Ok`.
#[test]
fn a_flipped_byte_in_a_checksummed_file_is_refused() {
    let Some(image) = nodatacow_image() else {
        eprintln!("skipping: .vm-share/btrfs-nodatacow.img not built");
        return;
    };
    let clean = read_clean(&image, "/cow.bin");
    assert!(clean.len() >= 32, "the fixture's file is too short to mark");

    let (fs, dev) = mount_flipping(&image, &clean[..32], true);
    let inode = fs.lookup_path("/cow.bin").expect("look up the file");
    let got = fs.read_file(inode.ino);

    assert!(
        dev.flipped(),
        "the wrapper never matched, so nothing was damaged and this test \
         would have passed whatever the driver did"
    );
    match got {
        Err(Error::ChecksumMismatch { what, .. }) => {
            assert_eq!(what, "a data extent", "the wrong structure was blamed");
        }
        Err(other) => panic!("a flipped data byte gave {other:?}"),
        Ok(bytes) => panic!(
            "a flipped data byte was returned as content: first byte {:#04x} where the \
             file holds {:#04x}",
            bytes[0], clean[0]
        ),
    }
}

/// The same flip on a `NODATASUM` file is served, because there is
/// nothing to check it against.
///
/// Half of the policy, and the half that is easy to get wrong in the
/// other direction: a driver that treated a missing digest as a failure
/// would refuse every file in a `chattr +C` directory, which the kernel
/// reads without complaint.
#[test]
fn a_flipped_byte_in_a_nodatasum_file_still_reads() {
    let Some(image) = nodatacow_image() else {
        eprintln!("skipping: .vm-share/btrfs-nodatacow.img not built");
        return;
    };
    let clean = read_clean(&image, "/nc/inplace.bin");
    assert!(clean.len() >= 32, "the fixture's file is too short to mark");

    let (fs, dev) = mount_flipping(&image, &clean[..32], true);
    let inode = fs.lookup_path("/nc/inplace.bin").expect("look up the file");
    let got = fs
        .read_file(inode.ino)
        .expect("a nodatasum file has nothing to verify against");

    assert!(
        dev.flipped(),
        "the wrapper never matched, so nothing was damaged"
    );
    assert_eq!(
        got[0],
        clean[0] ^ 0xFF,
        "the flip did not reach the bytes the driver returned"
    );
    assert_eq!(got[1..], clean[1..], "more than the flipped byte changed");
}

/// The wrapper itself changes nothing when it is disarmed.
///
/// Without this, both tests above rest on the assumption that mounting
/// through an extra `BlockRead` is transparent. It is the control for
/// the instrument rather than for the driver.
#[test]
fn the_wrapper_is_transparent_when_it_is_not_armed() {
    let Some(image) = nodatacow_image() else {
        eprintln!("skipping: .vm-share/btrfs-nodatacow.img not built");
        return;
    };
    for path in ["/cow.bin", "/nc/inplace.bin"] {
        let clean = read_clean(&image, path);
        let (fs, dev) = mount_flipping(&image, &clean[..32], false);
        let inode = fs.lookup_path(path).expect("look up the file");
        let got = fs.read_file(inode.ino).expect("an undamaged file reads");
        assert!(!dev.flipped(), "the disarmed wrapper flipped a byte");
        assert_eq!(got, clean, "{path} read differently through the wrapper");
    }
}

/// Every ordinary file on every fixture still reads, byte for byte,
/// with verification on.
///
/// The refusal tests say the check fires. This says it does not fire
/// where it should not — the failure that would arrive as "this driver
/// will not read my filesystem", which is worse for a user than the
/// crash it replaced. Four checksum algorithms are among these
/// fixtures, so it also covers the digest widths: a driver reading a
/// 32-byte sha256 digest four bytes at a time would fail here and
/// nowhere else.
#[test]
fn every_fixture_still_reads_with_verification_on() {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let Ok(entries) = std::fs::read_dir(&share) else {
        eprintln!("skipping: no fixtures built");
        return;
    };
    let mut checked = 0usize;
    for image in entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("img"))
        .filter(|p| !common::spans_several_devices(p))
    {
        let dev: Arc<dyn BlockRead> = match FileDevice::open(&image) {
            Ok(d) => Arc::new(d),
            Err(_) => continue,
        };
        let Ok(fs) = Filesystem::mount(dev) else {
            continue;
        };
        let Ok(root) = fs.lookup_path("/") else {
            continue;
        };
        let Ok(entries) = fs.read_dir(root.ino) else {
            continue;
        };
        for entry in entries {
            let name = String::from_utf8_lossy(&entry.name).into_owned();
            let Ok(inode) = fs.read_inode(entry.ino) else {
                continue;
            };
            if !inode.is_regular_file() || inode.size == 0 {
                continue;
            }
            match fs.read_file(entry.ino) {
                Ok(bytes) => {
                    assert_eq!(
                        bytes.len() as u64,
                        inode.size,
                        "{}: {name} read short",
                        image.display(),
                    );
                    checked += 1;
                }
                // A compressed extent this build cannot decode, or any
                // other refusal that is not about checksums, is not
                // this test's business — but a checksum failure on an
                // undamaged fixture is exactly what it is watching for.
                Err(Error::ChecksumMismatch { what, offset }) => panic!(
                    "{}: {name} failed its {what} checksum at {offset} on an undamaged image",
                    image.display(),
                ),
                Err(_) => continue,
            }
        }
    }
    assert!(
        checked > 0,
        "no file was read on any fixture, so this test asserted nothing"
    );
}
