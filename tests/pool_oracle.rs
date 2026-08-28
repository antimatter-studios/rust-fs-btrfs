//! A filesystem spanning two devices, opened with one.
//!
//! A chunk stripe names the device it lives on. With one device open
//! there is nothing sensible to do about a stripe naming another —
//! reading it from the device at hand returns whatever is at that
//! offset on THIS disk. It parses. It fails its checksum against a
//! block it was never meant to be, and on a mirrored pool, where both
//! disks hold the same data at different offsets, it may not even do
//! that.
//!
//! So the expected behaviour is a refusal, and this holds the driver to
//! it. Reading one disk of a pool as though it were the whole pool is
//! worse than not opening it: the caller gets data rather than an
//! error.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-pool-fixtures.sh`.

use fs_btrfs::fs::Filesystem;
use fs_core::FileDevice;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn pool_member(name: &str) -> Option<PathBuf> {
    let p = share().join(name);
    p.exists().then_some(p)
}

/// Opening one device of a two-device filesystem is refused.
#[test]
fn one_device_of_a_pool_is_refused_rather_than_half_read() {
    let members: Vec<PathBuf> = ["btrfs-pool-a.img", "btrfs-pool-b.img"]
        .iter()
        .filter_map(|n| pool_member(n))
        .collect();
    if members.is_empty() {
        eprintln!("no pool fixture; build it with ./scripts/vm-build-pool-fixtures.sh");
        return;
    }
    assert_eq!(
        members.len(),
        2,
        "a pool fixture needs both halves; found {}",
        members.len()
    );

    for path in &members {
        let dev = Arc::new(FileDevice::open(path).expect("opening a pool member"));
        // `Filesystem` is not Debug, so the error is taken out by hand
        // rather than through expect_err.
        let msg = match Filesystem::mount(dev) {
            Ok(_) => panic!(
                "{}: a two-device filesystem opened with ONE device was accepted. Every \
                 read of a chunk on the other device now returns whatever lies at that \
                 offset on this one.",
                path.display()
            ),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("spans 2 devices"),
            "the refusal should say what is wrong and how many devices are involved: {msg}"
        );
        eprintln!(
            "{}: refused — {msg}",
            path.file_name().unwrap().to_string_lossy()
        );
    }
}

/// The two halves really are one filesystem, and really are two devices.
///
/// Guards the fixture rather than the driver. If mkfs quietly produced
/// two independent single-device filesystems, the test above would pass
/// for the wrong reason — it would be refusing something that was never
/// a pool.
#[test]
fn the_fixture_really_is_one_filesystem_on_two_devices() {
    let (Some(a), Some(b)) = (
        pool_member("btrfs-pool-a.img"),
        pool_member("btrfs-pool-b.img"),
    ) else {
        eprintln!("no pool fixture — skipping");
        return;
    };

    let read = |p: &Path| -> ([u8; 16], u64, u64) {
        let raw = std::fs::read(p).expect("reading a pool member");
        let sb = &raw[0x10000..0x11000];
        let mut fsid = [0u8; 16];
        fsid.copy_from_slice(&sb[0x20..0x30]);
        (
            fsid,
            u64::from_le_bytes(sb[0x88..0x90].try_into().unwrap()),
            // The embedded dev_item's devid.
            u64::from_le_bytes(sb[0xc9..0xd1].try_into().unwrap()),
        )
    };

    let (fsid_a, devs_a, id_a) = read(&a);
    let (fsid_b, devs_b, id_b) = read(&b);

    assert_eq!(fsid_a, fsid_b, "the two images are not the same filesystem");
    assert_eq!(devs_a, 2, "device a does not say it is part of a pair");
    assert_eq!(devs_b, 2, "device b does not say it is part of a pair");
    assert_ne!(id_a, id_b, "both images claim to be the same device");
    eprintln!("one filesystem, devices {id_a} and {id_b}");
}
