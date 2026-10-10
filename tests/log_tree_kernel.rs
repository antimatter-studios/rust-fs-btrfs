//! An unreplayed log tree, read by this crate and judged by the kernel's
//! own replay of it (#266, first slice).
//!
//! The fixture (`chore fixtures dirtylog`) holds a log tree the kernel
//! wrote on `fsync` and never replayed: `durable.txt` overwritten and
//! `fsynced.txt` created after the last commit. The oracle is the kernel
//! mounting a copy read-write, which replays the log, and reporting each
//! file's size and contents. What this crate reads out of the log must be
//! what the kernel's replay produced, and the committed trees, read with
//! the log ignored, must still be the state before the `fsync`s.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::log_tree::LoggedTree;
use fs_btrfs_test_support::{fixture, guest_kernel_write_ok, temp_path};
use fs_core::{BlockRead, FileDevice};
use std::path::PathBuf;
use std::sync::Arc;

const DIRTY_LOG_FIXTURE: &str = "dirtylog/btrfs-dirty-log.img";
const COMMITTED: &[u8] = b"committed by a sync\n";

#[test]
fn the_log_holds_what_the_kernels_replay_produces() {
    let image = fixture(DIRTY_LOG_FIXTURE);
    let dev = Arc::new(FileDevice::open(&image).expect("open the fixture"));
    let fs = Filesystem::mount_ignoring_log(dev as Arc<dyn BlockRead>)
        .expect("a mount that ignores the log opens a volume with one");
    assert_ne!(
        fs.superblock().log_root,
        0,
        "the handle should report the log_root the device holds"
    );

    // The committed trees: the state before the fsyncs.
    assert_eq!(
        fs.read_path("/durable.txt")
            .expect("durable.txt is committed"),
        COMMITTED,
        "ignoring the log should read durable.txt as the last commit left it"
    );
    assert!(
        fs.read_path("/fsynced.txt").is_err(),
        "fsynced.txt was never committed, so the committed trees should not hold it"
    );

    // The log.
    let log = fs
        .log()
        .expect("read the log")
        .expect("the log is not empty");
    let top: &LoggedTree = log
        .trees
        .iter()
        .find(|t| t.subvolume == 5)
        .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
    let durable = fs.lookup_path("/durable.txt").unwrap().ino;
    let created = top
        .inode_named(256, b"fsynced.txt")
        .unwrap_or_else(|| panic!("the log names no fsynced.txt: {:?}", top.items));
    for ino in [durable, created] {
        assert!(
            top.inodes().contains(&ino),
            "the log holds no inode item for {ino}; it holds {:?}",
            top.inodes()
        );
    }
    let ours = [
        fs.logged_file(top, durable)
            .expect("durable.txt from the log"),
        fs.logged_file(top, created)
            .expect("fsynced.txt from the log"),
    ];

    // The oracle: the kernel replays a copy and reads both files back.
    let copy = PathBuf::from(temp_path!("log-tree-read-replayed.img"));
    std::fs::copy(&image, &copy).expect("copying the fixture to replay on");
    let replayed = guest_kernel_write_ok(
        &copy.to_string_lossy(),
        "log replay",
        "cat \"$MNT/durable.txt\"; printf '|'; cat \"$MNT/fsynced.txt\"",
    );
    let _ = std::fs::remove_file(&copy);
    let theirs: Vec<&[u8]> = replayed.as_bytes().splitn(2, |&b| b == b'|').collect();
    assert_eq!(theirs.len(), 2, "the kernel printed {replayed:?}");
    for (name, (ours, theirs)) in ["durable.txt", "fsynced.txt"]
        .iter()
        .zip(ours.iter().zip(theirs))
    {
        assert_eq!(
            String::from_utf8_lossy(ours),
            String::from_utf8_lossy(theirs),
            "{name}: the log read here differs from what the kernel's replay produced"
        );
    }
}
