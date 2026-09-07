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
///
/// Absent on a fresh clone, where these tests skip. Present wherever
/// the fixture has been built — and *required* wherever the caller says
/// it should be, which is what [`fixtures_are_required`] is for.
fn nodatacow_image() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".vm-share")
        .join("btrfs-nodatacow.img");
    if p.exists() {
        return Some(p);
    }
    assert!(
        !fixtures_are_required(),
        "BTRFS_ORACLE_FIXTURES=required, but {} is not there. The job that \
         sets that variable is the one that builds it, with \
         scripts/build-nodatacow-fixtures.sh — so either the build step is \
         missing or it failed silently.",
        p.display()
    );
    eprintln!("skipping: {} not built", p.display());
    None
}

/// Whether a missing fixture is a failure rather than a skip.
///
/// A skip reads exactly like a pass. Every test in this file opens with
/// an early return when its image is absent, and on a machine or a CI
/// job without the image that makes four green lines out of four bodies
/// that never ran — which is what happened to this file's first
/// revision: the whole mechanism it tests could be removed and nothing
/// went red.
///
/// So the caller that knows the fixture should be there says so.
/// `BTRFS_ORACLE_FIXTURES=required` is set by the CI job that builds
/// it, and by a developer who has built it and wants to know if the
/// harness quietly stopped finding it. Everywhere else — a fresh clone,
/// the fixture-less test job — the skip stands.
fn fixtures_are_required() -> bool {
    std::env::var("BTRFS_ORACLE_FIXTURES")
        .map(|v| v == "required")
        .unwrap_or(false)
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
        if !self.armed || buf.len() < self.marker.len() {
            return Ok(());
        }
        // The marker is looked for anywhere in the buffer, not only at
        // its start. A read is widened to sector boundaries before it
        // is verified, so the driver asks for a span that begins at the
        // start of the extent whatever the caller asked for — a marker
        // taken from the second sector then sits 4 KiB into the buffer,
        // and a wrapper matching only at position 0 would never fire.
        // That is not hypothetical: it is what the first version of
        // this did, and the test that needed it failed with "the
        // wrapper never matched" rather than passing quietly.
        if let Some(at) = buf
            .windows(self.marker.len())
            .position(|w| w == &self.marker[..])
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
/// one it replaced.
///
/// It does **not** cover the digest widths, and an earlier version of
/// this comment claimed it did. The `csum-sha256`, `csum-xxhash` and
/// `csum-blake2` fixtures are freshly-made filesystems with no ordinary
/// files on them, so this sweep never reads one: every file it does
/// read is crc32c, and fixing the digest length to four bytes fails
/// nothing here. That gap needs a populated non-crc32c fixture and is
/// filed as #102.
#[test]
fn every_fixture_still_reads_with_verification_on() {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let entries = match std::fs::read_dir(&share) {
        Ok(entries) => entries,
        Err(e) => {
            assert!(
                !fixtures_are_required(),
                "BTRFS_ORACLE_FIXTURES=required, but {} cannot be read: {e}",
                share.display()
            );
            eprintln!("skipping: no fixtures built");
            return;
        }
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

/// An unaligned read returns the caller's bytes, not the sector's.
///
/// Checksums cover whole sectors, so a read that starts or ends
/// mid-sector is widened to sector boundaries, verified, and then
/// sliced. Every read the rest of this file makes goes through
/// `read_file`, which starts at zero on a file whose length is a whole
/// number of sectors — so both halves of that widening are the identity
/// on every one of them, and a widening that was right for aligned
/// reads and wrong for unaligned ones would pass the whole suite.
///
/// The window here starts 100 bytes into a sector and ends 150 bytes
/// in, so neither end is aligned. Slicing the widened buffer from zero
/// rather than from the offset within it returns the sector's first
/// fifty bytes with `Ok` — wrong bytes, no error, which is the failure
/// this whole issue is about, one layer in.
#[test]
fn an_unaligned_read_of_a_checksummed_file_returns_the_bytes_asked_for() {
    let Some(image) = nodatacow_image() else {
        eprintln!("skipping: .vm-share/btrfs-nodatacow.img not built");
        return;
    };
    let clean = read_clean(&image, "/cow.bin");

    let dev: Arc<dyn BlockRead> = Arc::new(FileDevice::open(&image).expect("open fixture"));
    let fs = Filesystem::mount(dev).expect("mount fixture");
    let inode = fs.lookup_path("/cow.bin").expect("look up the file");

    let mut buf = [0u8; 50];
    let n = fs
        .read_at(inode.ino, 100, &mut buf)
        .expect("an unaligned read of an undamaged file");
    assert_eq!(n, buf.len());
    assert_eq!(
        &buf[..],
        &clean[100..150],
        "an unaligned window returned the wrong bytes"
    );

    // And one that starts mid-sector and runs past the end of it, so
    // the widening has to cover two sectors rather than one.
    let mut across = vec![0u8; 4096];
    let at = 4096 - 100;
    fs.read_at(inode.ino, at as u64, &mut across)
        .expect("a read spanning a sector boundary");
    assert_eq!(&across[..], &clean[at..at + across.len()]);
}

/// A flipped byte is refused even when the read does not start on the
/// sector holding it.
///
/// This is what the widening is *for*. Without it a mid-sector read has
/// no whole sector to compare against, and the choice is between
/// serving the fragment unchecked and refusing a read the kernel
/// serves. The window below starts in the sector before the damage and
/// ends inside the damaged one, so the sector that has to be verified
/// is neither the first the caller asked for nor one it asked for
/// wholly.
#[test]
fn an_unaligned_read_overlapping_damage_is_still_refused() {
    let Some(image) = nodatacow_image() else {
        eprintln!("skipping: .vm-share/btrfs-nodatacow.img not built");
        return;
    };
    let clean = read_clean(&image, "/cow.bin");
    // The marker is the second sector's first 32 bytes, so the flip
    // lands there rather than at the file's start.
    let marker = &clean[4096..4096 + 32];

    let (fs, dev) = mount_flipping(&image, marker, true);
    let inode = fs.lookup_path("/cow.bin").expect("look up the file");

    let mut buf = [0u8; 300];
    let got = fs.read_at(inode.ino, 4096 - 100, &mut buf);

    assert!(
        dev.flipped(),
        "the wrapper never matched the second sector, so nothing was damaged"
    );
    match got {
        Err(Error::ChecksumMismatch { what, .. }) => {
            assert_eq!(what, "a data extent", "the wrong structure was blamed");
        }
        Err(other) => panic!("a flipped byte in an overlapped sector gave {other:?}"),
        Ok(_) => panic!(
            "a read overlapping a damaged sector returned successfully, so the \
             widening does not cover the bytes actually served"
        ),
    }
}
