//! A repairing scrub leaves a volume the kernel's scrub and `btrfs check`
//! both find clean (#302).
//!
//! The fixture is `scrub/btrfs-scrub.img`: a DUP volume the kernel
//! populated, then three copies damaged on purpose — the second copy of a
//! data sector, the first copy of the next, and the second copy of the fs
//! tree's root block. Every one has a good twin.
//!
//! `scrub_repair` writes the good twin's bytes over each bad copy, with
//! no transaction. Three oracles judge the result: this crate's own scrub
//! must find nothing; the kernel's read-only scrub (`btrfs scrub start -B
//! -R -r`) must count no read, checksum or verify error; and `btrfs check
//! --readonly` must find the volume clean. A repair that wrote the wrong
//! copy, the wrong offset, or a block with a stale checksum fails at
//! least one of them.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::scrub::ScrubTarget;
use fs_btrfs_test_support::{assert_btrfs_check_clean, fixture, guest_kernel_read_ok, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::path::PathBuf;
use std::sync::Arc;

/// A copy of the damaged fixture, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-scrub-repair-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(fixture("scrub/btrfs-scrub.img"), dir.join("fs.img"))
            .expect("copying the scrub fixture");
        Self(dir)
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

/// A counter from `btrfs scrub start -R` output.
fn counter(report: &str, name: &str) -> u64 {
    report
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("the kernel's scrub printed no {name}:\n{report}"))
        .trim()
        .parse()
        .expect("a count")
}

#[test]
fn every_bad_copy_is_repaired_and_the_kernel_and_checker_agree() {
    let scratch = Scratch::new("all");
    let image = scratch.image();

    let dev = Arc::new(FileDevice::open_rw(&image).expect("open read-write"));
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write");
    let report = fs.scrub_repair().expect("scrub repair");
    assert_eq!(
        report.scrub.errors.len(),
        3,
        "the fixture damages three copies: {:#?}",
        report.scrub.errors
    );
    assert!(
        report.unrepairable.is_empty(),
        "every damaged copy has a good twin: {:#?}",
        report.unrepairable
    );
    let found: Vec<(ScrubTarget, u64, usize)> = report
        .scrub
        .errors
        .iter()
        .map(|e| (e.what, e.logical, e.mirror))
        .collect();
    assert_eq!(report.repaired, found, "each bad copy repaired once");
    drop(fs);

    // Our own scrub, on a fresh mount, finds nothing.
    let dev = Arc::new(FileDevice::open(&image).expect("open"));
    let again = Filesystem::mount(dev)
        .expect("mount the repaired volume")
        .scrub()
        .expect("scrub after the repair");
    assert!(again.errors.is_empty(), "{:#?}", again.errors);

    assert_btrfs_check_clean(&image, "after scrub repair");

    let kernel = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "kernel scrub after the repair",
        "btrfs scrub start -B -R -r \"$MNT\"",
    );
    for name in ["read_errors", "csum_errors", "verify_errors"] {
        assert_eq!(
            counter(&kernel, name),
            0,
            "the kernel's scrub still finds {name} after the repair:\n{kernel}"
        );
    }
}

/// A read-only mount repairs nothing and says so.
#[test]
fn a_read_only_mount_is_refused() {
    let dev = Arc::new(FileDevice::open(fixture("scrub/btrfs-scrub.img")).expect("open"));
    let fs = Filesystem::mount(dev).expect("mount");
    assert!(matches!(
        fs.scrub_repair(),
        Err(fs_btrfs::error::Error::ReadOnly)
    ));
}
