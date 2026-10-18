//! The guest kernel reads the exact bytes this crate writes into a seed
//! and sprout pair (#626), and btrfs-progs checks the complete pair.
//!
//! The seed fixtures come from the kernel. Only scratch copies are
//! written, and the seed's digest must stay unchanged across the crate's
//! commits. The pool helper registers both copies in the guest, checks
//! them, and mounts the sprout read-only before running the readback.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{fixture, guest_kernel_pool_read, sha256_hex, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn the_kernel_reads_back_a_file_written_into_a_seed_and_sprout_pair() {
    let seed_path = PathBuf::from(temp_path!("seed-kernel-seed.img"));
    let sprout_path = PathBuf::from(temp_path!("seed-kernel-sprout.img"));
    for (name, copy) in [
        ("btrfs-seed.img", &seed_path),
        ("btrfs-sprout.img", &sprout_path),
    ] {
        std::fs::copy(fixture(&format!("seed/{name}")), copy).expect("copy the member");
    }
    let seed_before = sha256_hex(&std::fs::read(&seed_path).expect("read the seed copy"));
    let seed = Arc::new(FileDevice::open_rw(&seed_path).expect("open the seed copy"))
        as Arc<dyn BlockDevice>;
    let sprout = Arc::new(FileDevice::open_rw(&sprout_path).expect("open the sprout copy"))
        as Arc<dyn BlockDevice>;
    let mut fs = Filesystem::mount_pool_rw(vec![seed, sprout]).expect("mount seed and sprout");
    let generation = fs.superblock().generation;
    let root = fs.lookup_path("/").expect("the top-level root").ino;
    let ino = fs
        .create(root, b"kernel-readback.bin", 0o644, 0, 0)
        .expect("create a file on the sprout");
    // Regular extents, with NULs and non-UTF-8 bytes: no text conversion
    // or digest comparison may stand in for reading every byte back.
    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        fs.write(ino, 0, &payload).expect("write the payload"),
        payload.len(),
        "the crate must commit the complete payload"
    );
    assert!(fs.superblock().generation > generation);
    drop(fs);
    assert_eq!(
        sha256_hex(&std::fs::read(&seed_path).expect("read the seed after committing")),
        seed_before,
        "the crate wrote the seed"
    );

    // The helper mounts its first member. The seed has its own fsid and
    // would mount as the original filesystem, so the sprout comes first.
    let images = [sprout_path.to_string_lossy(), seed_path.to_string_lossy()];
    let read = guest_kernel_pool_read(
        &[images[0].as_ref(), images[1].as_ref()],
        "cat \"$MNT/kernel-readback.bin\"",
    );
    assert_eq!(
        read.check_status, 0,
        "btrfs check --readonly rejected the written pair:\n{}",
        read.check
    );
    assert!(
        read.out.status.success(),
        "the guest kernel could not read the written pair:\n{}",
        String::from_utf8_lossy(&read.out.stderr)
    );
    assert_eq!(
        read.out.stdout.len(),
        payload.len(),
        "kernel readback length"
    );
    assert!(
        read.out.stdout == payload,
        "the guest kernel read different bytes from the file this crate wrote"
    );
}
