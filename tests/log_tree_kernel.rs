//! An unreplayed log tree, read and replayed by this crate and judged by
//! the kernel's own replay of it (#266).
//!
//! The fixtures (`chore fixtures dirtylog`) hold log trees the kernel
//! wrote on `fsync` and never replayed. The first has `durable.txt`
//! overwritten and `fsynced.txt` created after the last commit; the
//! second has data overwritten inside a committed extent, a file created,
//! one appended to, one truncated, and a directory whose names were
//! removed, renamed and added. The oracle is the kernel mounting a copy
//! read-write, which replays the log. What this crate reads out of the
//! log must be what the kernel's replay produced, and a copy this crate
//! replays must be what the kernel's is: every path, type, mode, size,
//! link count and file modification time, the contents of every file as
//! the kernel reads them back, and `btrfs check` clean on both.

use fs_btrfs::dir::{parse_dir_items, DIR_INDEX_KEY, XATTR_ITEM_KEY};
use fs_btrfs::fs::Filesystem;
use fs_btrfs::inode::{INODE_ITEM_KEY, INODE_REF_KEY};
use fs_btrfs::log_tree::{LoggedTree, DIR_LOG_INDEX_KEY};
use fs_btrfs::Error;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, fixture, guest_kernel_read_ok, guest_kernel_report,
    guest_kernel_write_ok, oracle, sha256_hex, temp_path,
};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DIRTY_LOG_FIXTURE: &str = "dirtylog/btrfs-dirty-log.img";
const REPLAY_FIXTURE: &str = "dirtylog/btrfs-dirty-log-replay.img";
const INODE_FIXTURE: &str = "dirtylog/btrfs-dirty-log-inode.img";
const COMMITTED: &[u8] = b"committed by a sync\n";
/// The native `dump-tree` transcript the fixture recipe took of the
/// inode-only crash image, inside the guest, before the image left it.
const INODE_TRANSCRIPT: &str = "dirtylog/btrfs-dirty-log-inode.logdump";
/// The native transcript the fixture recipe took of the data-and-names
/// crash image inside the guest: its sha256, `dump-super`, the log root
/// tree and the log tree that root points at.
const REPLAY_TRANSCRIPT: &str = "dirtylog/btrfs-dirty-log-replay.logdump";
/// Where the primary superblock starts on every btrfs volume.
const SUPERBLOCK: usize = 0x1_0000;

/// Whether a native `dump-tree` transcript describes `image` rather than
/// some other volume: every block header it prints must name the fsid in
/// the image's raw superblock bytes, and one of its blocks must be the
/// log root that superblock names. Nothing here goes through this crate's
/// parser, so the check cannot agree with a misreading of ours. Returns
/// the bound fsid, or why the transcript is not of this image.
fn transcript_binding(transcript: &str, image: &[u8]) -> Result<String, String> {
    let sb = image
        .get(SUPERBLOCK..SUPERBLOCK + 0x68)
        .ok_or("the image is too short to hold a superblock")?;
    if &sb[0x40..0x48] != b"_BHRfS_M" {
        return Err("the image has no btrfs superblock magic".into());
    }
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let f = &sb[0x20..0x30];
    let fsid = format!(
        "{}-{}-{}-{}-{}",
        hex(&f[0..4]),
        hex(&f[4..6]),
        hex(&f[6..8]),
        hex(&f[8..10]),
        hex(&f[10..16])
    );
    let log_root = u64::from_le_bytes(sb[0x60..0x68].try_into().unwrap());
    let uuids: Vec<&str> = transcript
        .lines()
        .filter_map(|line| line.trim().strip_prefix("fs uuid "))
        .map(str::trim)
        .collect();
    if uuids.is_empty() {
        return Err("the transcript declares no fs uuid".into());
    }
    if let Some(other) = uuids.iter().find(|uuid| **uuid != fsid) {
        return Err(format!(
            "the transcript's fs uuid {other} is not the image's {fsid}"
        ));
    }
    let blocks: Vec<u64> = transcript
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("leaf" | "node"), Some(bytenr)) => bytenr.parse().ok(),
                _ => None,
            }
        })
        .collect();
    if log_root == 0 || !blocks.contains(&log_root) {
        return Err(format!(
            "the transcript prints no block at the image's log root {log_root}"
        ));
    }
    Ok(fsid)
}

/// The kernel-side transcript is evidence about one image only. It binds
/// to the crash image it was taken of, and refuses another real volume,
/// a relabelled fsid and a transcript with its identity stripped.
#[test]
fn the_native_log_transcript_is_bound_to_its_crash_image() {
    let transcript =
        std::fs::read_to_string(fixture(INODE_TRANSCRIPT)).expect("read the native log transcript");
    let image = std::fs::read(fixture(INODE_FIXTURE)).expect("read the inode-only image");
    let fsid = transcript_binding(&transcript, &image)
        .unwrap_or_else(|why| panic!("{INODE_TRANSCRIPT} is not of {INODE_FIXTURE}: {why}"));
    let other = std::fs::read(fixture(REPLAY_FIXTURE)).expect("read another kernel-made image");
    assert!(
        transcript_binding(&transcript, &other).is_err(),
        "the transcript bound to {REPLAY_FIXTURE}, a different volume"
    );
    let relabelled = transcript.replace(&fsid, "00000000-0000-0000-0000-000000000000");
    assert!(
        transcript_binding(&relabelled, &image).is_err(),
        "a transcript relabelled to another fsid still bound"
    );
    let stripped: String = transcript
        .lines()
        .filter(|line| !line.trim().starts_with("fs uuid "))
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(
        transcript_binding(&stripped, &image).is_err(),
        "a transcript with no fs uuid still bound"
    );
    let digest = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(&image))
    };
    std::fs::create_dir_all("tmp/logs").expect("create transcript binding evidence directory");
    std::fs::write(
        "tmp/logs/inode-transcript-binding.txt",
        format!(
            "transcript: {INODE_TRANSCRIPT}\nimage: {INODE_FIXTURE}\n\
             image sha256: {digest}\nbound fsid: {fsid}\n"
        ),
    )
    .expect("retain transcript binding evidence");
}

/// The data-and-names transcript names the exact bytes it was taken of:
/// the guest's sha256 of the crash image must equal this image's, and its
/// block headers must carry the image's fsid and print its log root.
/// Another kernel-made volume, a relabelled fsid and a forged digest are
/// all refused.
#[test]
fn the_native_replay_transcript_is_bound_to_its_crash_image() {
    use sha2::Digest;
    let transcript = std::fs::read_to_string(fixture(REPLAY_TRANSCRIPT))
        .expect("read the native data-and-names transcript");
    let image = std::fs::read(fixture(REPLAY_FIXTURE)).expect("read the data-and-names image");
    let digest = format!("{:x}", sha2::Sha256::digest(&image));
    let names = |transcript: &str, digest: &str| {
        let named: Vec<&str> = transcript
            .lines()
            .filter_map(|line| line.strip_prefix("image sha256: "))
            .map(str::trim)
            .collect();
        named == [digest]
    };
    assert!(
        names(&transcript, &digest),
        "{REPLAY_TRANSCRIPT} was not taken of {REPLAY_FIXTURE} (sha256 {digest})"
    );
    let fsid = transcript_binding(&transcript, &image)
        .unwrap_or_else(|why| panic!("{REPLAY_TRANSCRIPT} is not of {REPLAY_FIXTURE}: {why}"));
    let other = std::fs::read(fixture(INODE_FIXTURE)).expect("read another kernel-made image");
    assert!(
        transcript_binding(&transcript, &other).is_err(),
        "the transcript bound to {INODE_FIXTURE}, a different volume"
    );
    let relabelled = transcript.replace(&fsid, "00000000-0000-0000-0000-000000000000");
    assert!(
        transcript_binding(&relabelled, &image).is_err(),
        "a transcript relabelled to another fsid still bound"
    );
    let other_digest = format!("{:x}", sha2::Sha256::digest(&other));
    assert_ne!(
        other_digest, digest,
        "the forged-digest control needs two different images"
    );
    let forged = transcript.replace(&digest, &other_digest);
    assert!(
        !names(&forged, &digest),
        "a transcript naming another image's digest still named this one"
    );
    let unnamed = transcript.replace("image sha256: ", "image: ");
    assert!(
        !names(&unnamed, &digest),
        "a transcript with no image digest still named this image"
    );
    std::fs::create_dir_all("tmp/logs").expect("create transcript binding evidence directory");
    std::fs::write(
        "tmp/logs/replay-transcript-binding.txt",
        format!(
            "transcript: {REPLAY_TRANSCRIPT}\nimage: {REPLAY_FIXTURE}\n\
             image sha256: {digest}\nbound fsid: {fsid}\n"
        ),
    )
    .expect("retain transcript binding evidence");
}

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

/// Every path's type, link count and size, every regular file's
/// modification time, every device's major and minor numbers (in hex, as
/// `stat` prints them), and the inode of every name whose inode has more
/// than one, so that two names are the same inode only if the kernel says
/// so, as the kernel reports them for the mounted image.
///
/// A directory's times are left out: a name the replay removes moves its
/// directory's times to the moment of the replay, in the kernel's replay
/// as in this crate's, and the two replays happen at different moments.
const STATS: &str = r#"
cd "$MNT"
find . -mindepth 1 -printf 'stat\t%P\t%y %n %s\n' | sort
find . -mindepth 1 -type f -printf 'mtime\t%P\t%T@\n' | sort
find . -mindepth 1 \( -type b -o -type c \) -printf '%P\n' | sort | while read -r p; do
    printf 'rdev\t%s\t%s\n' "$p" "$(stat -c '%t:%T' "$p")"
done
find . -mindepth 1 ! -type d -links +1 -printf 'ino\t%P\t%i\n' | sort
"#;

/// `kind<TAB>path<TAB>value` lines, keyed by `(kind, path)`.
fn parse(report: &str) -> BTreeMap<(String, String), String> {
    report
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let kind = fields.next().unwrap_or_default().to_string();
            let path = fields.next().unwrap_or_default().to_string();
            let value = fields.next().unwrap_or_default().to_string();
            ((kind, path), value)
        })
        .collect()
}

/// What the kernel sees in `image`: its report of every path, and the
/// link counts, sizes and file times of [`STATS`].
fn kernel_view(image: &Path, what: &str) -> BTreeMap<(String, String), String> {
    let image = image.to_string_lossy();
    let mut view = guest_kernel_report(&image, what);
    view.extend(parse(&guest_kernel_read_ok(&image, what, STATS)));
    view
}

/// Every key on which two views differ, with both values.
fn differences(
    ours: &BTreeMap<(String, String), String>,
    theirs: &BTreeMap<(String, String), String>,
) -> Vec<String> {
    let mut keys: Vec<&(String, String)> = ours.keys().chain(theirs.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| ours.get(*k) != theirs.get(*k))
        .map(|k| {
            format!(
                "{} {}: ours {:?}, the kernel's {:?}",
                k.0,
                k.1,
                ours.get(k),
                theirs.get(k)
            )
        })
        .collect()
}

/// Replay a copy of `fixture_name` with the kernel and another with this
/// crate, and require the two to be the same volume to the kernel.
/// Returns the kernel's view of its own replay, for a test that also
/// needs to know what that view holds.
fn replays_as_the_kernel_does(fixture_name: &str, tag: &str) -> BTreeMap<(String, String), String> {
    replays_as_the_kernel_does_then(fixture_name, tag, |_, _| {})
}

/// [`replays_as_the_kernel_does`], handing the kernel's replayed copy and
/// this crate's to `inspect`, in that order, before they are removed, for
/// a test that compares what the kernel's view does not show.
fn replays_as_the_kernel_does_then(
    fixture_name: &str,
    tag: &str,
    inspect: impl FnOnce(&Path, &Path),
) -> BTreeMap<(String, String), String> {
    let image = fixture(fixture_name);

    let theirs = PathBuf::from(temp_path!("{tag}-kernel-replay.img"));
    std::fs::copy(&image, &theirs).expect("copying the fixture for the kernel to replay");
    guest_kernel_write_ok(
        &theirs.to_string_lossy(),
        &format!("{tag}: kernel replay"),
        "true",
    );

    let ours = PathBuf::from(temp_path!("{tag}-our-replay.img"));
    std::fs::copy(&image, &ours).expect("copying the fixture for this crate to replay");
    {
        let dev = Arc::new(FileDevice::open_rw(&ours).expect("open the copy read-write"));
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>)
            .unwrap_or_else(|e| panic!("[{tag}] a read-write mount should replay the log: {e}"));
        assert_eq!(
            fs.superblock().log_root,
            0,
            "[{tag}] the mount replayed the log and still names one"
        );
    }
    let reopened = Filesystem::mount(Arc::new(
        FileDevice::open(&ours).expect("open the replayed copy"),
    ) as Arc<dyn BlockRead>);
    assert!(
        !matches!(reopened, Err(Error::DirtyLog)),
        "[{tag}] the replay was not committed: the copy still holds a log"
    );

    assert_btrfs_check_clean(&theirs, &format!("{tag}: the kernel's replay"));
    assert_btrfs_check_clean(&ours, &format!("{tag}: this crate's replay"));
    let theirs_view = kernel_view(&theirs, &format!("{tag}: the kernel's replay"));
    let ours_view = kernel_view(&ours, &format!("{tag}: this crate's replay"));
    let differ = differences(&ours_view, &theirs_view);
    // What the views show differing is reported first.
    if differ.is_empty() {
        inspect(&theirs, &ours);
    }
    let _ = std::fs::remove_file(&theirs);
    let _ = std::fs::remove_file(&ours);
    assert!(
        differ.is_empty(),
        "[{tag}] the volume this crate replayed differs from the kernel's replay:\n{}",
        differ.join("\n")
    );
    theirs_view
}

/// `durable.txt` rewritten in place and `fsynced.txt` created, both made
/// durable by fsync alone (#376): the new file exists only in the log, as
/// its inode item, its reference to `/` and its inline extent, with no
/// directory item for it. The kernel's replay is the oracle.
#[test]
fn a_read_write_mount_replays_the_log_as_the_kernel_does() {
    replays_as_the_kernel_does(DIRTY_LOG_FIXTURE, "dirtylog");
}

/// The second fixture's log, checked to hold each change its recipe made
/// before the replay is judged: a fixture whose fsyncs fell back to a
/// commit would hold nothing to replay, and pass for the wrong reason.
#[test]
fn a_log_of_data_and_names_replays_as_the_kernel_does() {
    let image = fixture(REPLAY_FIXTURE);
    {
        let dev = Arc::new(FileDevice::open(&image).expect("open the fixture"));
        let fs = Filesystem::mount_ignoring_log(dev as Arc<dyn BlockRead>)
            .expect("a mount that ignores the log opens a volume with one");
        let log = fs
            .log()
            .expect("read the log")
            .expect("the log is not empty");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        for path in ["/big.bin", "/grow.bin", "/shrink.bin"] {
            let ino = fs.lookup_path(path).unwrap().ino;
            assert!(
                top.inodes().contains(&ino),
                "the log holds no inode item for {path} (inode {ino}): {keys:?}"
            );
        }
        assert!(
            top.inode_named(256, b"new.bin").is_some(),
            "the log names no new.bin: {keys:?}"
        );
        let d = fs.lookup_path("/d").unwrap().ino;
        assert!(
            top.items
                .iter()
                .any(|i| i.key.objectid == d && i.key.key_type == DIR_LOG_INDEX_KEY),
            "the log holds no index range for directory d (inode {d}): {keys:?}"
        );
    }
    replays_as_the_kernel_does(REPLAY_FIXTURE, "dirtylog-replay");
}

/// Prove this is a metadata-only recovery before asking mount_rw to
/// replay it. Unchanged inode refs may be logged, but no data, checksum,
/// xattr, directory entry or directory deletion range is permitted.
#[test]
fn an_inode_only_log_replays_mode_and_identity_as_the_kernel_does() {
    let image = fixture(INODE_FIXTURE);
    let transcript =
        std::fs::read_to_string(fixture(INODE_TRANSCRIPT)).expect("read the native log transcript");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .unwrap_or_else(|why| panic!("{INODE_TRANSCRIPT} is not of {INODE_FIXTURE}: {why}"));
    let committed = Filesystem::mount_ignoring_log(Arc::new(
        FileDevice::open(&image).expect("open inode-only fixture"),
    ) as Arc<dyn BlockRead>)
    .expect("read committed inode without replay");
    let before = committed.lookup_path("/metadata-only").unwrap();
    assert_eq!(
        (before.mode, before.uid, before.gid, before.size),
        (0o100600, 12345, 23456, 0)
    );
    let log = committed.log().unwrap().expect("a dirty inode-only log");
    assert_eq!(
        log.trees.len(),
        1,
        "only the top-level subvolume was changed"
    );
    let top = &log.trees[0];
    assert_eq!(top.subvolume, 5);
    assert!(
        top.items
            .iter()
            .all(|item| matches!(item.key.key_type, INODE_ITEM_KEY | INODE_REF_KEY)),
        "metadata-only fixture contains other records: {:?}",
        top.items
    );
    for item in top
        .items
        .iter()
        .filter(|item| item.key.key_type == INODE_REF_KEY)
    {
        assert_eq!((item.key.objectid, item.key.offset), (before.ino, 256));
        assert_eq!(top.inode_named(256, b"metadata-only"), Some(before.ino));
    }
    let logged = top.inode(before.ino).unwrap().expect("logged chmod inode");
    assert_eq!(
        (logged.mode, logged.uid, logged.gid, logged.size),
        (0o100640, 12345, 23456, 0)
    );
    assert_eq!(
        (logged.ino, logged.generation, logged.nlink),
        (before.ino, before.generation, before.nlink)
    );
    let records: Vec<_> = top
        .items
        .iter()
        .map(|item| (item.key, item.data.len()))
        .collect();
    drop(committed);

    let theirs = PathBuf::from(temp_path!("inode-only-kernel.img"));
    let ours = PathBuf::from(temp_path!("inode-only-crate.img"));
    std::fs::copy(&image, &theirs).unwrap();
    std::fs::copy(&image, &ours).unwrap();
    let digest = |path: &std::path::Path| {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(std::fs::read(path).unwrap()))
    };
    let crash_image = digest(&image);
    assert_eq!(
        digest(&theirs),
        crash_image,
        "kernel copy is the crash image"
    );
    assert_eq!(digest(&ours), crash_image, "crate copy is the crash image");
    const IDENTITY: &str = "stat -c '%i %a %u %g %h %s' \"$MNT/metadata-only\"";
    let expected = format!("{} 640 12345 23456 1 0", before.ino);
    let kernel = guest_kernel_write_ok(
        &theirs.to_string_lossy(),
        "inode-only kernel replay",
        IDENTITY,
    );
    assert_eq!(
        kernel.trim(),
        expected,
        "the real kernel recovered chmod and identity"
    );
    let kernel_fs =
        Filesystem::mount(Arc::new(FileDevice::open(&theirs).unwrap()) as Arc<dyn BlockRead>)
            .expect("kernel cleared its log on disk");
    assert_eq!(kernel_fs.superblock().log_root, 0);
    let recovered = kernel_fs.lookup_path("/metadata-only").unwrap();
    // Keep independent recovery evidence even when our mount refuses the
    // log. Both CI paths retain tmp/logs, including on a failing tier.
    std::fs::create_dir_all("tmp/logs").expect("create inode replay evidence directory");
    let evidence = "tmp/logs/inode-only-replay.txt";
    std::fs::write(
        evidence,
        format!(
            "fixture: {INODE_FIXTURE}\ncrash image sha256: {crash_image}\n\
             log root: {}\nrecords: {records:?}\n\
             committed: {before:?}\nlogged: {logged:?}\n\
             kernel stat: {}\nkernel recovered: {recovered:?}\n",
            log.log_root,
            kernel.trim(),
        ),
    )
    .expect("retain inode replay evidence");
    drop(kernel_fs);
    assert_btrfs_check_clean(&theirs, "inode-only kernel replay before crate mount");
    {
        let fs = Filesystem::mount_rw(
            Arc::new(FileDevice::open_rw(&ours).unwrap()) as Arc<dyn BlockDevice>
        )
        .unwrap_or_else(|error| {
            panic!(
                "an inode-only read-write mount must replay chmod: {error:?}; \
                 kernel recovered {} and passed btrfs check; evidence: {evidence}",
                kernel.trim(),
            )
        });
        assert_eq!(fs.superblock().log_root, 0);
        let replayed = fs.lookup_path("/metadata-only").unwrap();
        assert_eq!(
            (replayed.mode, replayed.uid, replayed.gid, replayed.nlink),
            (
                recovered.mode,
                recovered.uid,
                recovered.gid,
                recovered.nlink
            ),
            "the crate recovered the mode and identity the kernel recovered"
        );
        assert_eq!(replayed, logged);
    }
    let reopened =
        Filesystem::mount(Arc::new(FileDevice::open(&ours).unwrap()) as Arc<dyn BlockRead>)
            .expect("inode-only replay and log clearing persist");
    assert_eq!(reopened.superblock().log_root, 0);
    assert_eq!(reopened.lookup_path("/metadata-only").unwrap(), logged);
    drop(reopened);
    assert_btrfs_check_clean(&theirs, "inode-only kernel replay");
    assert_btrfs_check_clean(&ours, "inode-only crate replay");
    let actual = guest_kernel_read_ok(
        &ours.to_string_lossy(),
        "inode-only crate readback",
        IDENTITY,
    );
    assert_eq!(
        actual.trim(),
        kernel.trim(),
        "kernel observes the same recovered inode"
    );
    assert!(differences(
        &kernel_view(&ours, "inode-only crate"),
        &kernel_view(&theirs, "inode-only kernel")
    )
    .is_empty());
}

const INLINE_FIXTURE: &str = "dirtylog/btrfs-dirty-log-inline.img";
/// The native transcript of the inline crash image, headed by the guest's
/// sha256 of that never-remounted image.
const INLINE_TRANSCRIPT: &str = "dirtylog/btrfs-dirty-log-inline.logdump";

/// A scratch copy this test made, removed when the guard drops: on
/// success, on an early `?`/`expect` failure and on panic unwinding. It
/// owns only the path it created, so retained fixtures and other runs'
/// files are never touched.
struct ScratchCopy(PathBuf);

impl ScratchCopy {
    fn of(source: &Path, path: String) -> ScratchCopy {
        let guard = ScratchCopy(PathBuf::from(path));
        std::fs::copy(source, &guard.0).expect("copy the fixture to scratch");
        guard
    }
}

impl Drop for ScratchCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The inline transcript's single `image sha256:` header, if it names
/// exactly `image`, and its superblock binding.
fn inline_transcript_binding(transcript: &str, image: &[u8]) -> Result<String, String> {
    use sha2::Digest;
    let digest = format!("{:x}", sha2::Sha256::digest(image));
    let named: Vec<&str> = transcript
        .lines()
        .filter_map(|line| line.strip_prefix("image sha256: "))
        .map(str::trim)
        .collect();
    if named != [digest.as_str()] {
        return Err(format!("the transcript names {named:?}, not {digest}"));
    }
    transcript_binding(transcript, image)
}

/// The positive inline transcript binds to its image by digest and
/// superblock, and refuses another kernel-made image, another shape's
/// transcript, a forged digest and a stripped header.
#[test]
fn the_native_inline_transcript_is_bound_to_its_crash_image() {
    use sha2::Digest;
    let transcript = std::fs::read_to_string(fixture(INLINE_TRANSCRIPT))
        .expect("the native transcript of the inline overwrite log");
    let image = std::fs::read(fixture(INLINE_FIXTURE)).unwrap();
    inline_transcript_binding(&transcript, &image)
        .unwrap_or_else(|why| panic!("{INLINE_TRANSCRIPT} is not of {INLINE_FIXTURE}: {why}"));
    let other = std::fs::read(fixture("dirtylog/btrfs-dirty-log-sizechange.img")).unwrap();
    assert!(
        inline_transcript_binding(&transcript, &other).is_err(),
        "the inline transcript bound to another kernel-made image"
    );
    let wrong =
        std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-sizechange.logdump")).unwrap();
    assert!(
        inline_transcript_binding(&wrong, &image).is_err(),
        "another shape's transcript bound to the inline image"
    );
    let digest = format!("{:x}", sha2::Sha256::digest(&image));
    let other_digest = format!("{:x}", sha2::Sha256::digest(&other));
    assert!(
        inline_transcript_binding(&transcript.replace(&digest, &other_digest), &image).is_err(),
        "a forged digest still bound"
    );
    assert!(
        inline_transcript_binding(&transcript.replace("image sha256: ", "image: "), &image)
            .is_err(),
        "a transcript with no digest header still bound"
    );
}
const INLINE_COMMITTED: &[u8] = b"committed-inline-bytes-0123456789";
const INLINE_LOGGED: &[u8] = b"replayed-inline-bytes-ABCDEFGHIJK";

/// A same-length overwrite of an existing inline file, made durable by
/// fsync alone. The kernel's replay is the oracle for the bytes, the
/// identity and a clean btrfs check; the crate must reach the same
/// state on its own copy of the same crash image.
#[test]
fn a_same_length_inline_overwrite_replays_as_the_kernel_does() {
    let image = fixture(INLINE_FIXTURE);
    let transcript = std::fs::read_to_string(fixture(INLINE_TRANSCRIPT)).unwrap();
    inline_transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the positive transcript is of this exact image");
    let committed = Filesystem::mount_ignoring_log(Arc::new(
        FileDevice::open(&image).expect("open inline overwrite fixture"),
    ) as Arc<dyn BlockRead>)
    .expect("read committed inline file without replay");
    let before = committed.lookup_path("/inline-overwrite").unwrap();
    assert_eq!(before.size, INLINE_COMMITTED.len() as u64);
    // Same length, or the committed tail survives the overwrite and the
    // file the kernel recovers is not the logged text alone.
    assert_eq!(INLINE_LOGGED.len(), INLINE_COMMITTED.len());
    assert_eq!(committed.read_file(before.ino).unwrap(), INLINE_COMMITTED);
    let log = committed
        .log()
        .unwrap()
        .expect("a dirty inline overwrite log");
    assert_eq!(log.trees.len(), 1, "only the top-level subvolume changed");
    let top = &log.trees[0];
    assert_eq!(top.subvolume, 5);
    assert!(
        top.items.iter().all(|item| matches!(
            item.key.key_type,
            INODE_ITEM_KEY | INODE_REF_KEY | fs_btrfs::fs::EXTENT_DATA_KEY
        )),
        "inline overwrite fixture contains other records: {:?}",
        top.items
    );
    assert!(
        top.items
            .iter()
            .any(|item| (item.key.objectid, item.key.key_type)
                == (before.ino, fs_btrfs::fs::EXTENT_DATA_KEY)),
        "the overwrite's file extent is logged"
    );
    drop(committed);

    let theirs_guard = ScratchCopy::of(&image, temp_path!("inline-kernel.img"));
    let ours_guard = ScratchCopy::of(&image, temp_path!("inline-crate.img"));
    let (theirs, ours) = (theirs_guard.0.clone(), ours_guard.0.clone());
    const VIEW: &str =
        "stat -c '%i %s %h' \"$MNT/inline-overwrite\"; cat \"$MNT/inline-overwrite\"";
    let expected = format!(
        "{} {} 1\n{}",
        before.ino,
        INLINE_LOGGED.len(),
        String::from_utf8_lossy(INLINE_LOGGED)
    );
    let kernel = guest_kernel_write_ok(&theirs.to_string_lossy(), "inline kernel replay", VIEW);
    assert_eq!(
        kernel.trim(),
        expected,
        "the real kernel recovered the overwrite"
    );
    assert_btrfs_check_clean(&theirs, "inline kernel replay");
    {
        let fs = Filesystem::mount_rw(
            Arc::new(FileDevice::open_rw(&ours).unwrap()) as Arc<dyn BlockDevice>
        )
        .unwrap_or_else(|error| {
            panic!(
                "a same-length inline overwrite log must replay: {error:?}; \
                 kernel recovered {}",
                kernel.trim()
            )
        });
        assert_eq!(fs.superblock().log_root, 0);
        let replayed = fs.lookup_path("/inline-overwrite").unwrap();
        assert_eq!(replayed.ino, before.ino);
        assert_eq!(fs.read_file(replayed.ino).unwrap(), INLINE_LOGGED);
    }
    assert_btrfs_check_clean(&ours, "inline crate replay");
    let actual = guest_kernel_read_ok(&ours.to_string_lossy(), "inline crate readback", VIEW);
    assert_eq!(
        actual.trim(),
        expected,
        "the kernel reads the crate's replay"
    );
}

/// A kernel-made log of a shape replay does not support must be refused
/// by `mount_rw` with `DirtyLog`, and the refusal must write nothing: the
/// whole image hashes the same before and after. The transcript is the
/// native `dump-tree` of that exact image, bound by sha256 and superblock.
fn an_unsupported_log_is_refused_without_writing(shape: &str, damage: impl FnOnce(&Path)) {
    use sha2::Digest;
    let image = fixture(&format!("dirtylog/btrfs-dirty-log-{shape}.img"));
    let transcript = std::fs::read_to_string(fixture(&format!(
        "dirtylog/btrfs-dirty-log-{shape}.logdump"
    )))
    .expect("the native transcript of the unsupported log");
    let bytes = std::fs::read(&image).unwrap();
    transcript_binding(&transcript, &bytes).expect("transcript bound to this image");
    let guard = ScratchCopy::of(&image, temp_path!("refused-{shape}.img"));
    let copy = guard.0.clone();
    damage(&copy);
    let before = format!("{:x}", sha2::Sha256::digest(std::fs::read(&copy).unwrap()));
    let result =
        Filesystem::mount_rw(Arc::new(FileDevice::open_rw(&copy).unwrap()) as Arc<dyn BlockDevice>);
    assert!(
        matches!(result, Err(Error::DirtyLog)),
        "{shape}: an unsupported log must be refused with DirtyLog"
    );
    drop(result);
    let after = format!("{:x}", sha2::Sha256::digest(std::fs::read(&copy).unwrap()));
    assert_eq!(before, after, "{shape}: the refusal wrote to the image");
}

/// An inline rewrite of `/f` logged with a name added to `/` (#377,
/// #378): `g` created, renamed to `h`, and `/` fsynced, so the log holds
/// `/` itself, the index range it speaks for, and `h`'s entry in it,
/// which names a file the log also creates.
#[test]
fn a_mixed_names_and_inline_log_replays_as_the_kernel_does() {
    an_inline_rewrite_replays_as_the_kernel_does("mixedlog", b"replayed-inline-bytes-ABCDEFGHIJK");
}

/// A log whose data did not reach the disk as logged (#375): one byte of
/// the extent the log gives `new.bin` is changed on a copy of the crash
/// image, so the extent's bytes no longer match the digests the log
/// carries for them. Replay must refuse to record data the log does not
/// vouch for, before anything is written.
#[test]
fn a_log_whose_data_disagrees_with_its_checksums_is_refused_without_writing() {
    an_unsupported_log_is_refused_without_writing("replay", |copy| {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(copy).expect("open the copy"),
        ) as Arc<dyn BlockRead>)
        .expect("read the copy without replay");
        let log = fs.log().unwrap().expect("the copy holds a log");
        let top = &log.trees[0];
        let ino = top
            .inode_named(256, b"new.bin")
            .expect("the log creates new.bin");
        let extent = top
            .items
            .iter()
            .find(|i| i.key.objectid == ino && i.key.key_type == fs_btrfs::fs::EXTENT_DATA_KEY)
            .expect("the log holds new.bin's extent");
        let logical = u64::from_le_bytes(extent.data[21..29].try_into().unwrap());
        drop(fs);
        // Where the extent is on the device, from the native tool's dump
        // of the chunk tree: the chunk holding it, and that chunk's one
        // stripe on this single-device volume.
        let chunks = dump_tree(copy, "chunk");
        let (mut start, mut length) = (None, None);
        let mut physical = None;
        for line in chunks.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            let after = |name: &str| {
                words
                    .iter()
                    .position(|w| *w == name)
                    .and_then(|at| words.get(at + 1))
                    .and_then(|w| w.trim_end_matches(')').parse::<u64>().ok())
            };
            if line.contains("CHUNK_ITEM") {
                start = after("CHUNK_ITEM");
                length = None;
            } else if let Some(n) = after("length") {
                length = Some(n);
            } else if let (Some(true), Some(offset), Some(s), Some(len)) = (
                line.trim_start().starts_with("stripe 0").then_some(true),
                after("offset"),
                start,
                length,
            ) {
                if (s..s + len).contains(&logical) {
                    physical = Some(offset + (logical - s));
                }
            }
        }
        let physical = physical
            .unwrap_or_else(|| panic!("no chunk in the dump holds logical {logical}:\n{chunks}"));
        let mut bytes = std::fs::read(copy).unwrap();
        bytes[physical as usize] ^= 0xff;
        std::fs::write(copy, bytes).unwrap();
    });
}

/// A logged directory whose names the log speaks for by hashed ranges
/// (`DIR_LOG_ITEM`, over `DIR_ITEM` name hashes) rather than by index
/// ranges (#408). The CI guest's kernel (Debian 12, 6.1) never writes
/// one: in sixteen fsync shapes, adding, removing and renaming names in
/// committed and new directories, every log it left held `DIR_INDEX`
/// items and `DIR_LOG_INDEX` ranges and no `DIR_ITEM` or `DIR_LOG_ITEM`
/// at all. With no kernel replay of one to judge this crate's against,
/// replay keeps refusing the shape before anything is written. The
/// kernel-made directory-fsync log has each index range of `/` retyped as
/// a hashed range, in place on a copy, its leaf still in key order and
/// given a fresh checksum.
#[test]
fn a_log_with_a_hashed_directory_range_is_refused_without_writing() {
    an_unsupported_log_is_refused_without_writing("dirlink", |copy| {
        use fs_btrfs::btree::{Body, HEADER_SIZE, ITEM_SIZE};
        use fs_btrfs::log_tree::DIR_LOG_ITEM_KEY;
        use std::os::unix::fs::FileExt;
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(copy).expect("open the copy"),
        ) as Arc<dyn BlockRead>)
        .expect("read the copy without replay");
        let log = fs.log().unwrap().expect("the copy holds a log");
        let top = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .expect("the copy holds a log of the top-level subvolume");
        let block = fs.read_tree_block(top.root).unwrap();
        let Body::Leaf(items) = &block.body else {
            panic!("the directory-fsync log is expected to be one leaf");
        };
        let mut bytes = block.bytes().to_vec();
        let mut keys = Vec::with_capacity(items.len());
        for (n, item) in items.iter().enumerate() {
            let mut key_type = item.key.key_type;
            if (item.key.objectid, key_type) == (256, DIR_LOG_INDEX_KEY) {
                // An item's key type is the byte after its 8-byte objectid.
                key_type = DIR_LOG_ITEM_KEY;
                bytes[HEADER_SIZE + n * ITEM_SIZE + 8] = key_type;
            }
            keys.push((item.key.objectid, key_type, item.key.offset));
        }
        assert!(
            keys.iter()
                .any(|&(o, t, _)| (o, t) == (256, DIR_LOG_ITEM_KEY)),
            "the directory-fsync log holds no index range of /"
        );
        assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "retyping the ranges left the leaf out of key order: {keys:?}"
        );
        fs_btrfs::tree_write::stamp_checksum(&mut bytes, fs.superblock());
        let file = std::fs::OpenOptions::new().write(true).open(copy).unwrap();
        for mirror in 0..fs.chunk_map().mirrors_at(top.root).unwrap() {
            let m = fs.chunk_map().map_mirror(top.root, mirror).unwrap();
            file.write_all_at(&bytes, m.physical).unwrap();
        }
    });
}

/// An inline rewrite of `/f`, made durable by fsync alone, that #364's
/// same-length replay does not cover (#378): the kernel replays one copy
/// of the crash image and `mount_rw` the other. Both must pass `btrfs
/// check`, hold the logged bytes, and be the same volume to the kernel.
fn an_inline_rewrite_replays_as_the_kernel_does(shape: &str, logged: &[u8]) {
    use sha2::Digest;
    let image = fixture(&format!("dirtylog/btrfs-dirty-log-{shape}.img"));
    let transcript = std::fs::read_to_string(fixture(&format!(
        "dirtylog/btrfs-dirty-log-{shape}.logdump"
    )))
    .expect("the native transcript of the inline rewrite log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    let committed = Filesystem::mount_ignoring_log(Arc::new(
        FileDevice::open(&image).expect("open the inline rewrite fixture"),
    ) as Arc<dyn BlockRead>)
    .expect("read the committed file without replay");
    let before = committed.lookup_path("/f").unwrap();
    assert_eq!(
        committed.read_file(before.ino).unwrap(),
        b"committed-inline-bytes-0123456789",
        "[{shape}] the committed file is the bytes written before the rewrite"
    );
    drop(committed);

    let theirs_guard = ScratchCopy::of(&image, temp_path!("{shape}-kernel.img"));
    let ours_guard = ScratchCopy::of(&image, temp_path!("{shape}-crate.img"));
    let (theirs, ours) = (theirs_guard.0.clone(), ours_guard.0.clone());
    const VIEW: &str = "stat -c '%i %s %h' \"$MNT/f\"; sha256sum \"$MNT/f\" | cut -d' ' -f1";
    let expected = format!(
        "{} {} 1\n{:x}",
        before.ino,
        logged.len(),
        sha2::Sha256::digest(logged)
    );
    let kernel = guest_kernel_write_ok(
        &theirs.to_string_lossy(),
        &format!("{shape} kernel replay"),
        VIEW,
    );
    assert_eq!(
        kernel.trim(),
        expected,
        "[{shape}] the real kernel recovered the rewrite"
    );
    assert_btrfs_check_clean(&theirs, &format!("{shape} kernel replay"));
    {
        let fs = Filesystem::mount_rw(
            Arc::new(FileDevice::open_rw(&ours).unwrap()) as Arc<dyn BlockDevice>
        )
        .unwrap_or_else(|error| {
            panic!(
                "[{shape}] an inline rewrite log must replay: {error:?}; kernel recovered {}",
                kernel.trim()
            )
        });
        assert_eq!(fs.superblock().log_root, 0);
        let replayed = fs.lookup_path("/f").unwrap();
        assert_eq!(replayed.ino, before.ino);
        assert_eq!(fs.read_file(replayed.ino).unwrap(), logged);
    }
    assert_btrfs_check_clean(&ours, &format!("{shape} crate replay"));
    let actual = guest_kernel_read_ok(
        &ours.to_string_lossy(),
        &format!("{shape} crate readback"),
        VIEW,
    );
    assert_eq!(
        actual.trim(),
        expected,
        "[{shape}] the kernel reads the crate's replay"
    );
    assert!(differences(
        &kernel_view(&ours, &format!("{shape} crate")),
        &kernel_view(&theirs, &format!("{shape} kernel"))
    )
    .is_empty());
}

/// A compressed inline extent in place of an uncompressed one (#378):
/// `compress-force=zlib` stores 33 bytes of `z` compressed, and replay
/// must copy the encoded bytes and their compression type as logged.
#[test]
fn a_compressed_inline_rewrite_replays_as_the_kernel_does() {
    an_inline_rewrite_replays_as_the_kernel_does("compressedinline", &[b'z'; 33]);
}

const EXTERNAL_FIXTURE: &str = "dirtylog/btrfs-dirty-log-externalextent.img";
const EXTERNAL_TRANSCRIPT: &str = "dirtylog/btrfs-dirty-log-externalextent.logdump";
/// The fixture's file: 8 KiB of `c` in one regular, checksummed extent,
/// committed, then overwritten whole with `r` and fsynced.
const EXTERNAL_LEN: usize = 8192;

/// `btrfs check --check-data-csum`: every data sector against its digest,
/// which `btrfs check --readonly` alone does not read.
fn assert_data_csums_clean(image: &Path, tag: &str) {
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "[{tag}] btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
}

/// An existing file overwritten whole and made durable by fsync alone
/// (#375): the log holds the file's new regular extent, which no
/// committed extent record names yet, and that extent's checksums. This
/// was refused until replay could take the extent into the extent tree
/// and its digests into the checksum tree; the kernel's replay of the same
/// crash image is the oracle for what the file and the trees end up as.
#[test]
fn an_external_extent_overwrite_replays_as_the_kernel_does() {
    use sha2::Digest;
    let image = fixture(EXTERNAL_FIXTURE);
    let transcript = std::fs::read_to_string(fixture(EXTERNAL_TRANSCRIPT))
        .expect("the native transcript of the external extent log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    let committed = Filesystem::mount_ignoring_log(Arc::new(
        FileDevice::open(&image).expect("open the external extent fixture"),
    ) as Arc<dyn BlockRead>)
    .expect("read the committed file without replay");
    let before = committed.lookup_path("/f").unwrap();
    assert_eq!(
        committed.read_file(before.ino).unwrap(),
        vec![b'c'; EXTERNAL_LEN],
        "the committed file is the bytes written before the overwrite"
    );
    let log = committed
        .log()
        .unwrap()
        .expect("a dirty external extent log");
    assert_eq!(log.trees.len(), 1, "only the top-level subvolume changed");
    let top = &log.trees[0];
    assert!(
        top.items
            .iter()
            .any(|item| (item.key.objectid, item.key.key_type)
                == (before.ino, fs_btrfs::fs::EXTENT_DATA_KEY)),
        "the overwrite's file extent is logged"
    );
    assert!(
        top.items
            .iter()
            .any(|item| item.key.key_type == fs_btrfs::csum::EXTENT_CSUM_KEY),
        "the new extent's checksums are logged"
    );
    drop(committed);

    let theirs_guard = ScratchCopy::of(&image, temp_path!("external-kernel.img"));
    let ours_guard = ScratchCopy::of(&image, temp_path!("external-crate.img"));
    let (theirs, ours) = (theirs_guard.0.clone(), ours_guard.0.clone());
    const VIEW: &str = "stat -c '%i %s %h' \"$MNT/f\"; sha256sum \"$MNT/f\" | cut -d' ' -f1";
    let expected = format!(
        "{} {EXTERNAL_LEN} 1\n{:x}",
        before.ino,
        sha2::Sha256::digest(vec![b'r'; EXTERNAL_LEN])
    );
    let kernel = guest_kernel_write_ok(
        &theirs.to_string_lossy(),
        "external extent kernel replay",
        VIEW,
    );
    assert_eq!(
        kernel.trim(),
        expected,
        "the real kernel recovered the overwrite"
    );
    assert_btrfs_check_clean(&theirs, "external extent kernel replay");
    assert_data_csums_clean(&theirs, "external extent kernel replay");
    {
        let fs = Filesystem::mount_rw(
            Arc::new(FileDevice::open_rw(&ours).unwrap()) as Arc<dyn BlockDevice>
        )
        .unwrap_or_else(|error| {
            panic!(
                "an external extent overwrite log must replay: {error:?}; kernel recovered {}",
                kernel.trim()
            )
        });
        assert_eq!(fs.superblock().log_root, 0);
        let replayed = fs.lookup_path("/f").unwrap();
        assert_eq!(replayed.ino, before.ino);
        assert_eq!(
            fs.read_file(replayed.ino).unwrap(),
            vec![b'r'; EXTERNAL_LEN]
        );
    }
    assert_btrfs_check_clean(&ours, "external extent crate replay");
    assert_data_csums_clean(&ours, "external extent crate replay");
    let actual = guest_kernel_read_ok(
        &ours.to_string_lossy(),
        "external extent crate readback",
        VIEW,
    );
    assert_eq!(
        actual.trim(),
        expected,
        "the kernel reads the crate's replay"
    );
    assert!(differences(
        &kernel_view(&ours, "external extent crate"),
        &kernel_view(&theirs, "external extent kernel")
    )
    .is_empty());
}

const COMPRESSED_CUT_FIXTURE: &str = "dirtylog/btrfs-dirty-log-compressedcut.img";
const COMPRESSED_CUT_TRANSCRIPT: &str = "dirtylog/btrfs-dirty-log-compressedcut.logdump";
/// The fixture's file: 128 KiB of `c` in one compressed extent, committed,
/// then one sector of `r` written at [`COMPRESSED_CUT_AT`] and fsynced.
const COMPRESSED_CUT_LEN: usize = 131072;
const COMPRESSED_CUT_AT: usize = 32768;

/// A sector written inside a committed compressed extent and made durable
/// by fsync alone (#409): the log holds only the new sector's extent, so
/// replay must leave the committed extent's head and tail as windows into
/// the whole compressed extent, without decoding it. The kernel's replay
/// of the same crash image is the oracle for the volume, its data
/// checksums and the file's bytes.
#[test]
fn a_write_inside_a_compressed_extent_replays_as_the_kernel_does() {
    use sha2::Digest;
    let image = fixture(COMPRESSED_CUT_FIXTURE);
    let transcript = std::fs::read_to_string(fixture(COMPRESSED_CUT_TRANSCRIPT))
        .expect("the native transcript of the compressed cut log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    let committed = Filesystem::mount_ignoring_log(Arc::new(
        FileDevice::open(&image).expect("open the compressed cut fixture"),
    ) as Arc<dyn BlockRead>)
    .expect("read the committed file without replay");
    let ino = committed.lookup_path("/f").unwrap().ino;
    assert_eq!(
        committed.read_file(ino).unwrap(),
        vec![b'c'; COMPRESSED_CUT_LEN],
        "the committed file is the bytes written before the logged sector"
    );
    drop(committed);

    replays_as_the_kernel_does(COMPRESSED_CUT_FIXTURE, "dirtylog-compressedcut");

    let theirs_guard = ScratchCopy::of(&image, temp_path!("compressedcut-kernel.img"));
    let ours_guard = ScratchCopy::of(&image, temp_path!("compressedcut-crate.img"));
    let (theirs, ours) = (theirs_guard.0.clone(), ours_guard.0.clone());
    let mut bytes = vec![b'c'; COMPRESSED_CUT_LEN];
    bytes[COMPRESSED_CUT_AT..COMPRESSED_CUT_AT + 4096].fill(b'r');
    const VIEW: &str = "stat -c '%i %s %h' \"$MNT/f\"; sha256sum \"$MNT/f\" | cut -d' ' -f1";
    let expected = format!(
        "{ino} {COMPRESSED_CUT_LEN} 1\n{:x}",
        sha2::Sha256::digest(&bytes)
    );
    let kernel = guest_kernel_write_ok(
        &theirs.to_string_lossy(),
        "compressed cut kernel replay",
        VIEW,
    );
    assert_eq!(
        kernel.trim(),
        expected,
        "the real kernel recovered the sector"
    );
    assert_data_csums_clean(&theirs, "compressed cut kernel replay");
    {
        let fs = Filesystem::mount_rw(
            Arc::new(FileDevice::open_rw(&ours).unwrap()) as Arc<dyn BlockDevice>
        )
        .unwrap_or_else(|error| panic!("a compressed cut log must replay: {error:?}"));
        assert_eq!(fs.read_file(ino).unwrap(), bytes);
    }
    assert_data_csums_clean(&ours, "compressed cut crate replay");
    let actual = guest_kernel_read_ok(
        &ours.to_string_lossy(),
        "compressed cut crate readback",
        VIEW,
    );
    assert_eq!(
        actual.trim(),
        expected,
        "the kernel reads the crate's replay"
    );
}

/// An inline extent that grows from 33 bytes to 43 (#378): the file's
/// size and `nbytes` move with it.
#[test]
fn a_size_changing_inline_rewrite_replays_as_the_kernel_does() {
    an_inline_rewrite_replays_as_the_kernel_does(
        "sizechange",
        b"replayed-and-longer-inline-bytes-0123456789",
    );
}

/// Fsynced changes in the top level and in a subvolume (#380): the log
/// root tree names a log tree for each, and replay must fold both into
/// their subvolumes in one transaction, as the kernel does.
#[test]
fn logs_of_two_subvolumes_replay_as_the_kernel_does() {
    const SUBVOL_FIXTURE: &str = "dirtylog/btrfs-dirty-log-subvol.img";
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(fixture(SUBVOL_FIXTURE)).expect("open the subvolume fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let log = fs.log().unwrap().expect("a dirty log");
        let subvolumes: Vec<u64> = log.trees.iter().map(|t| t.subvolume).collect();
        assert!(
            subvolumes.len() == 2 && subvolumes.contains(&5),
            "the log holds a log tree for the top level and one for the subvolume: {subvolumes:?}"
        );
    }
    replays_as_the_kernel_does(SUBVOL_FIXTURE, "dirtylog-subvol");
}

/// Extended attributes changed, removed and added on a committed file,
/// made durable by fsync alone (#379): the log holds the file's
/// attributes as they stand. The kernel's replay is the oracle for the
/// names and values the file is left with.
#[test]
fn logged_xattr_changes_replay_as_the_kernel_does() {
    let image = fixture("dirtylog/btrfs-dirty-log-xattr.img");
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-xattr.logdump"))
        .expect("the native transcript of the xattr log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    let committed = Filesystem::mount_ignoring_log(Arc::new(
        FileDevice::open(&image).expect("open the xattr fixture"),
    ) as Arc<dyn BlockRead>)
    .expect("read the committed file without replay");
    let before = committed.lookup_path("/f").unwrap();
    let log = committed.log().unwrap().expect("a dirty xattr log");
    assert!(
        log.trees[0]
            .items
            .iter()
            .any(|i| i.key.objectid == before.ino && i.key.key_type == XATTR_ITEM_KEY),
        "the log holds the file's attributes"
    );
    drop(committed);

    let theirs_guard = ScratchCopy::of(&image, temp_path!("xattr-kernel.img"));
    let ours_guard = ScratchCopy::of(&image, temp_path!("xattr-crate.img"));
    let (theirs, ours) = (theirs_guard.0.clone(), ours_guard.0.clone());
    const VIEW: &str = "python3 -c 'import os, sys; p = sys.argv[1]; \
        print(sorted((k, os.getxattr(p, k).decode()) for k in os.listxattr(p)))' \"$MNT/f\"";
    let expected =
        "[('user.keep', 'changed after the commit'), ('user.new', 'added after the commit')]";
    let kernel = guest_kernel_write_ok(&theirs.to_string_lossy(), "xattr kernel replay", VIEW);
    assert_eq!(
        kernel.trim(),
        expected,
        "the real kernel recovered the attributes"
    );
    assert_btrfs_check_clean(&theirs, "xattr kernel replay");
    {
        let fs = Filesystem::mount_rw(
            Arc::new(FileDevice::open_rw(&ours).unwrap()) as Arc<dyn BlockDevice>
        )
        .unwrap_or_else(|error| {
            panic!(
                "an xattr log must replay: {error:?}; kernel recovered {}",
                kernel.trim()
            )
        });
        assert_eq!(fs.superblock().log_root, 0);
    }
    assert_btrfs_check_clean(&ours, "xattr crate replay");
    let actual = guest_kernel_read_ok(&ours.to_string_lossy(), "xattr crate readback", VIEW);
    assert_eq!(
        actual.trim(),
        expected,
        "the kernel reads the crate's replay"
    );
    assert!(differences(
        &kernel_view(&ours, "xattr crate"),
        &kernel_view(&theirs, "xattr kernel")
    )
    .is_empty());
}

/// Directories made after the last commit, durable by an fsync of `/`
/// alone (#403): `a`, empty, and `b`, holding an empty file `x` and a
/// directory `c` of its own. The committed trees hold none of them; the
/// log holds `/`'s new entries and an inode item for each directory it
/// creates. The kernel's replay of the same crash image is the oracle for
/// every path the volume is left with.
#[test]
fn a_log_creating_directories_replays_as_the_kernel_does() {
    const NEWDIR_FIXTURE: &str = "dirtylog/btrfs-dirty-log-newdir.img";
    let image = fixture(NEWDIR_FIXTURE);
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-newdir.logdump"))
        .expect("the native transcript of the new-directory log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the new-directory fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        for path in ["/a", "/b"] {
            assert!(
                fs.lookup_path(path).is_err(),
                "{path} was made after the commit, so the committed trees should not hold it"
            );
        }
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        for name in ["a", "b"] {
            let ino = top
                .inode_named(256, name.as_bytes())
                .unwrap_or_else(|| panic!("the log names no {name} in /: {keys:?}"));
            let logged = top
                .inode(ino)
                .unwrap()
                .unwrap_or_else(|| panic!("the log holds no inode item for {name}: {keys:?}"));
            assert!(
                logged.is_dir(),
                "the log's {name} is not a directory: {logged:?}"
            );
        }
    }
    replays_as_the_kernel_does(NEWDIR_FIXTURE, "dirtylog-newdir");
}

/// A symbolic link `l` made after the last commit, durable by an fsync of
/// `/` alone (#404). The committed trees do not hold it; the log holds
/// `/`'s new entry, the link's inode item, its reference and the inline
/// extent that holds its target. The kernel's replay of the same crash
/// image is the oracle, down to the target each copy reads back.
#[test]
fn a_log_creating_a_symlink_replays_as_the_kernel_does() {
    const SYMLINK_FIXTURE: &str = "dirtylog/btrfs-dirty-log-symlink.img";
    let image = fixture(SYMLINK_FIXTURE);
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-symlink.logdump"))
        .expect("the native transcript of the symlink log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the symlink fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        assert!(
            fs.lookup_path("/l").is_err(),
            "/l was made after the commit, so the committed trees should not hold it"
        );
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        let ino = top
            .inode_named(256, b"l")
            .unwrap_or_else(|| panic!("the log names no l in /: {keys:?}"));
        let logged = top
            .inode(ino)
            .unwrap()
            .unwrap_or_else(|| panic!("the log holds no inode item for l: {keys:?}"));
        assert!(
            logged.is_symlink(),
            "the log's l is not a symbolic link: {logged:?}"
        );
        assert!(
            top.items
                .iter()
                .any(|i| (i.key.objectid, i.key.key_type, i.key.offset)
                    == (ino, fs_btrfs::fs::EXTENT_DATA_KEY, 0)),
            "the log holds no extent for the target of l: {keys:?}"
        );
    }
    replays_as_the_kernel_does(SYMLINK_FIXTURE, "dirtylog-symlink");
}

/// A character device `c` (240:300), a block device `b` (241:5), a FIFO
/// `p` and a UNIX socket `s`, made after the last commit and durable by
/// an fsync of `/` alone (#405). The committed trees hold none of them;
/// the log holds `/`'s new entries and each node's inode item, carrying
/// its type and device number, and its reference, with no data. The
/// kernel's replay of the same crash image is the oracle, down to the
/// type and the device numbers it reports for each node in both copies.
#[test]
fn a_log_creating_device_fifo_and_socket_nodes_replays_as_the_kernel_does() {
    use fs_btrfs::inode::FileType;
    const SPECIAL_FIXTURE: &str = "dirtylog/btrfs-dirty-log-special.img";
    // Each node, its type, and its device number as the kernel keeps it
    // on disk: major << 20 | minor.
    let nodes: [(&str, FileType, u64); 4] = [
        ("c", FileType::CharDevice, (240 << 20) | 300),
        ("b", FileType::BlockDevice, (241 << 20) | 5),
        ("p", FileType::Fifo, 0),
        ("s", FileType::Socket, 0),
    ];
    let image = fixture(SPECIAL_FIXTURE);
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-special.logdump"))
        .expect("the native transcript of the special-node log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the special-node fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        for (name, kind, rdev) in &nodes {
            assert!(
                fs.lookup_path(&format!("/{name}")).is_err(),
                "/{name} was made after the commit, so the committed trees should not hold it"
            );
            let ino = top
                .inode_named(256, name.as_bytes())
                .unwrap_or_else(|| panic!("the log names no {name} in /: {keys:?}"));
            let logged = top
                .inode(ino)
                .unwrap()
                .unwrap_or_else(|| panic!("the log holds no inode item for {name}: {keys:?}"));
            assert_eq!(
                (logged.file_type(), logged.rdev, logged.size),
                (Some(*kind), *rdev, 0),
                "the log's {name} is not the node the recipe made: {logged:?}"
            );
            assert!(
                !top.items
                    .iter()
                    .any(|i| (i.key.objectid, i.key.key_type)
                        == (ino, fs_btrfs::fs::EXTENT_DATA_KEY)),
                "the log holds data for {name}, a node that has none: {keys:?}"
            );
        }
    }
    let theirs = replays_as_the_kernel_does(SPECIAL_FIXTURE, "dirtylog-special");
    // The two replays agree; this is what they agree on, as the kernel
    // reports it, so a node both left out cannot pass.
    for (name, kind, rdev) in [
        ("c", "character-special-file", Some("f0:12c")),
        ("b", "block-special-file", Some("f1:5")),
        ("p", "fifo", None),
        ("s", "socket", None),
    ] {
        let get = |k: &str| {
            theirs
                .get(&(k.to_string(), name.to_string()))
                .map(String::as_str)
        };
        assert_eq!(get("type"), Some(kind), "the kernel's replay of {name}");
        assert_eq!(get("rdev"), rdev, "the kernel's replay of {name}");
    }
}

/// The committed file `f` given a second name `g`, and a file `n`
/// created with data and given a second name `m`, after the last commit
/// and made durable by an fsync of `f` and then one of `n` (#406). The
/// committed trees hold `f` alone, with one link. The log holds `f` in
/// full, its link count raised to 2 and its reference to `/` holding both
/// names; `n` with its data and both its names; and the entries of `/`
/// that name `g`, `n` and `m`. The kernel's replay of the same crash
/// image is the oracle, down to each name's link count and inode.
#[test]
fn a_log_adding_hard_links_replays_as_the_kernel_does() {
    const HARDLINK_FIXTURE: &str = "dirtylog/btrfs-dirty-log-hardlink.img";
    let image = fixture(HARDLINK_FIXTURE);
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-hardlink.logdump"))
        .expect("the native transcript of the hard-link log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the hard-link fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let committed = fs.lookup_path("/f").expect("the committed trees hold /f");
        assert_eq!(committed.nlink, 1, "/f is committed with one name");
        for path in ["/g", "/n", "/m"] {
            assert!(
                fs.lookup_path(path).is_err(),
                "{path} was made after the commit, so the committed trees should not hold it"
            );
        }
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        let named = |name: &str| {
            top.inode_named(256, name.as_bytes())
                .unwrap_or_else(|| panic!("the log names no {name} in /: {keys:?}"))
        };
        assert_eq!(
            (named("f"), named("g")),
            (committed.ino, committed.ino),
            "the log names f and g as the committed f: {keys:?}"
        );
        let n = named("n");
        assert_eq!(
            named("m"),
            n,
            "the log names n and m as one inode: {keys:?}"
        );
        let logged_f = top
            .inode(committed.ino)
            .unwrap()
            .expect("f's logged inode item");
        assert_eq!(
            (logged_f.generation, logged_f.nlink),
            (committed.generation, 2),
            "the log holds the committed f in full, with two links: {logged_f:?}"
        );
        let logged_n = top.inode(n).unwrap().expect("n's logged inode item");
        assert_eq!(
            (logged_n.nlink, logged_n.size),
            (2, 19),
            "the log holds n with two links and its data: {logged_n:?}"
        );
        let entries: Vec<(Vec<u8>, u64)> = top
            .names(256)
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.ino))
            .collect();
        for (name, ino) in [("g", committed.ino), ("n", n), ("m", n)] {
            assert!(
                entries.contains(&(name.as_bytes().to_vec(), ino)),
                "the log holds no entry of / naming {name}: {entries:?}"
            );
        }
    }
    let theirs = replays_as_the_kernel_does(HARDLINK_FIXTURE, "dirtylog-hardlink");
    // The two replays agree; this is what they agree on, as the kernel
    // reports it, so a name both left out cannot pass.
    let get = |kind: &str, name: &str| {
        theirs
            .get(&(kind.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("the kernel's replay reports no {kind} for {name}"))
    };
    for (first, second, size) in [("f", "g", 33), ("n", "m", 19)] {
        for name in [first, second] {
            assert_eq!(
                get("stat", name),
                format!("f 2 {size}"),
                "the kernel's replay of {name}"
            );
        }
        assert_eq!(
            get("ino", first),
            get("ino", second),
            "the kernel's replay names {first} and {second} as one inode"
        );
        assert_eq!(
            get("sha256", first),
            get("sha256", second),
            "{first} and {second} read back differently"
        );
    }
}

/// The committed file `f`, with 64 more names in `/` of 255 bytes each,
/// given a 66th after the last commit and made durable by an fsync of `/`
/// (#407). Its `INODE_REF` to `/` holds `f` and the first 61 long names,
/// as many as the largest item a 16 KiB leaf holds, so the kernel commits
/// the other three as `INODE_EXTREF` items and stores the new name in one
/// more. The committed trees hold `f` with 65 links. The log holds `f`
/// at its committed generation with 66 links, its `INODE_REF` and all
/// four extended references, and the entry of `/` naming the new one.
/// The kernel's replay of the same crash image is the oracle.
#[test]
fn a_log_adding_an_extended_reference_replays_as_the_kernel_does() {
    const EXTREF_FIXTURE: &str = "dirtylog/btrfs-dirty-log-extref.img";
    /// `BTRFS_INODE_EXTREF_KEY`.
    const INODE_EXTREF_KEY: u8 = 13;
    let long = |i: usize| format!("{i:03}").repeat(85);
    let image = fixture(EXTREF_FIXTURE);
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-extref.logdump"))
        .expect("the native transcript of the extended-reference log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the extended-reference fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let committed = fs.lookup_path("/f").expect("the committed trees hold /f");
        assert_eq!(committed.nlink, 65, "/f is committed with 65 names");
        for i in [0, 60, 61, 63] {
            let path = format!("/{}", long(i));
            let ino = fs
                .lookup_path(&path)
                .unwrap_or_else(|e| panic!("the committed trees should hold name {i}: {e}"))
                .ino;
            assert_eq!(ino, committed.ino, "committed name {i} is f");
        }
        assert!(
            fs.lookup_path(&format!("/{}", long(64))).is_err(),
            "the 66th name was made after the commit, so the committed trees should not hold it"
        );
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        // Every record of f's logged extended references, read from the
        // raw items: a parent, an index in it, and a name.
        let mut records: Vec<(u64, u64, Vec<u8>)> = Vec::new();
        for item in top
            .items
            .iter()
            .filter(|i| i.key.objectid == committed.ino && i.key.key_type == INODE_EXTREF_KEY)
        {
            let d = &item.data;
            let mut at = 0;
            while at < d.len() {
                let le64 = |p: usize| u64::from_le_bytes(d[p..p + 8].try_into().unwrap());
                let len = usize::from(u16::from_le_bytes(d[at + 16..at + 18].try_into().unwrap()));
                records.push((le64(at), le64(at + 8), d[at + 18..at + 18 + len].to_vec()));
                at += 18 + len;
            }
        }
        let mut names: Vec<String> = records
            .iter()
            .map(|r| String::from_utf8_lossy(&r.2).into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            (61..65).map(long).collect::<Vec<_>>(),
            "the log holds f's extended references to names 61 to 64: {keys:?}"
        );
        assert!(
            records.iter().all(|r| r.0 == 256),
            "every extended reference of f names / as its parent: {records:?}"
        );
        let index = records
            .iter()
            .find(|r| r.2 == long(64).as_bytes())
            .map(|r| r.1)
            .unwrap();
        let entry = top
            .items
            .iter()
            .find(|i| (i.key.objectid, i.key.key_type, i.key.offset) == (256, DIR_INDEX_KEY, index))
            .unwrap_or_else(|| panic!("the log holds no entry of / at index {index}: {keys:?}"));
        let entry = parse_dir_items(&entry.data).unwrap();
        assert_eq!(
            entry
                .iter()
                .map(|e| (e.ino, e.name.clone()))
                .collect::<Vec<_>>(),
            vec![(committed.ino, long(64).into_bytes())],
            "the logged entry of / at index {index} names f by the new name"
        );
        let logged_f = top
            .inode(committed.ino)
            .unwrap()
            .expect("f's logged inode item");
        assert_eq!(
            (logged_f.generation, logged_f.nlink),
            (committed.generation, 66),
            "the log holds the committed f with 66 links: {logged_f:?}"
        );
    }
    let theirs = replays_as_the_kernel_does(EXTREF_FIXTURE, "dirtylog-extref");
    // The two replays agree; this is what they agree on, as the kernel
    // reports it, so a name both left out cannot pass.
    let get = |kind: &str, name: &str| {
        theirs
            .get(&(kind.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("the kernel's replay reports no {kind} for {name}"))
    };
    for name in ["f".to_string(), long(0), long(61), long(64)] {
        assert_eq!(
            get("stat", &name),
            "f 66 33",
            "the kernel's replay of {name}"
        );
        assert_eq!(
            get("ino", &name),
            get("ino", "f"),
            "the kernel's replay names {name} as f"
        );
        assert_eq!(
            get("sha256", &name),
            get("sha256", "f"),
            "{name} reads back differently from f"
        );
    }
}

/// The committed file `f` given a second name `g` after the last commit,
/// made durable by an fsync of `/` alone (#461). The committed trees hold
/// `f` alone, with one link. The log holds the entry of `/` naming `g`,
/// in an index range of `/` that starts past `f`'s committed index, and
/// `f` only as an inode that exists: generation 0, two links, no data,
/// and its reference to `/` holding both names, the committed one at an
/// index the log does not speak for. The kernel's replay of the same
/// crash image is the oracle, down to each name's link count and inode.
#[test]
fn a_hard_link_made_durable_by_its_directory_replays_as_the_kernel_does() {
    const DIRLINK_FIXTURE: &str = "dirtylog/btrfs-dirty-log-dirlink.img";
    let image = fixture(DIRLINK_FIXTURE);
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-dirlink.logdump"))
        .expect("the native transcript of the directory-fsync hard-link log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the directory-fsync hard-link fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let committed = fs.lookup_path("/f").expect("the committed trees hold /f");
        assert_eq!(committed.nlink, 1, "/f is committed with one name");
        assert!(
            fs.lookup_path("/g").is_err(),
            "/g was made after the commit, so the committed trees should not hold it"
        );
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        // f's logged reference to /, read from the raw item: each name's
        // index in / and the name.
        let reference = top
            .items
            .iter()
            .find(|i| {
                (i.key.objectid, i.key.key_type, i.key.offset)
                    == (committed.ino, INODE_REF_KEY, 256)
            })
            .unwrap_or_else(|| panic!("the log holds no reference of f to /: {keys:?}"));
        let mut records: Vec<(u64, Vec<u8>)> = Vec::new();
        let d = &reference.data;
        let mut at = 0;
        while at < d.len() {
            let len = usize::from(u16::from_le_bytes(d[at + 8..at + 10].try_into().unwrap()));
            records.push((
                u64::from_le_bytes(d[at..at + 8].try_into().unwrap()),
                d[at + 10..at + 10 + len].to_vec(),
            ));
            at += 10 + len;
        }
        let index_of = |name: &str| {
            records
                .iter()
                .find(|r| r.1 == name.as_bytes())
                .map(|r| r.0)
                .unwrap_or_else(|| panic!("f's logged reference holds no {name}: {records:?}"))
        };
        assert_eq!(
            records.len(),
            2,
            "f's logged reference holds f and g: {records:?}"
        );
        let (at_f, at_g) = (index_of("f"), index_of("g"));
        let logged_f = top
            .inode(committed.ino)
            .unwrap()
            .expect("f's logged inode item");
        assert_eq!(
            (logged_f.generation, logged_f.nlink),
            (0, 2),
            "the log holds f only as an inode that exists, with two links: {logged_f:?}"
        );
        assert!(
            !top.items.iter().any(|i| (i.key.objectid, i.key.key_type)
                == (committed.ino, fs_btrfs::fs::EXTENT_DATA_KEY)),
            "the log holds no data of f: {keys:?}"
        );
        let entry_at = |index: u64| {
            top.items
                .iter()
                .find(|i| {
                    (i.key.objectid, i.key.key_type, i.key.offset) == (256, DIR_INDEX_KEY, index)
                })
                .map(|i| parse_dir_items(&i.data).unwrap())
        };
        assert_eq!(
            entry_at(at_g)
                .unwrap_or_else(|| panic!("the log holds no entry of / at index {at_g}: {keys:?}"))
                .iter()
                .map(|e| (e.ino, e.name.clone()))
                .collect::<Vec<_>>(),
            vec![(committed.ino, b"g".to_vec())],
            "the logged entry of / at index {at_g} names f by g"
        );
        assert!(
            entry_at(at_f).is_none(),
            "the log holds an entry of / at f's committed index {at_f}: {keys:?}"
        );
        let ranges: Vec<(u64, u64)> = top
            .items
            .iter()
            .filter(|i| i.key.objectid == 256 && i.key.key_type == DIR_LOG_INDEX_KEY)
            .map(|i| {
                (
                    i.key.offset,
                    u64::from_le_bytes(i.data[..8].try_into().unwrap()),
                )
            })
            .collect();
        let in_range = |index: u64| ranges.iter().any(|&(a, b)| a <= index && index <= b);
        assert!(
            in_range(at_g) && !in_range(at_f),
            "a logged index range of / holds g at {at_g} and none holds f at {at_f}: {ranges:?}"
        );
    }
    let theirs = replays_as_the_kernel_does(DIRLINK_FIXTURE, "dirtylog-dirlink");
    // The two replays agree; this is what they agree on, as the kernel
    // reports it, so a name both left out cannot pass.
    let get = |kind: &str, name: &str| {
        theirs
            .get(&(kind.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("the kernel's replay reports no {kind} for {name}"))
    };
    for name in ["f", "g"] {
        assert_eq!(get("stat", name), "f 2 33", "the kernel's replay of {name}");
    }
    assert_eq!(
        get("ino", "f"),
        get("ino", "g"),
        "the kernel's replay names f and g as one inode"
    );
    assert_eq!(
        get("sha256", "f"),
        get("sha256", "g"),
        "f and g read back differently"
    );
}

/// The committed file `f`, with 64 more names in `/` of 255 bytes each,
/// three of them in `INODE_EXTREF` items, rewritten in place with 33
/// other bytes and made durable by an fsync of `f` alone (#462). The log
/// holds `f` in full, at its committed generation with its 65 links and
/// its new inline extent, but the kernel leaves the logged `nbytes`
/// holding stale bytes of a long name rather than a count. The kernel's
/// replay counts `nbytes` again from the data, so its replay of the same
/// crash image is the oracle, with `btrfs check` judging the count.
#[test]
fn a_log_whose_inode_holds_a_stale_nbytes_replays_as_the_kernel_does() {
    const NBYTES_FIXTURE: &str = "dirtylog/btrfs-dirty-log-nbytes.img";
    const REWRITTEN: &[u8] = b"replayed-inline-bytes-ABCDEFGHIJK";
    let long = |i: usize| format!("{i:03}").repeat(85);
    let image = fixture(NBYTES_FIXTURE);
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-nbytes.logdump"))
        .expect("the native transcript of the stale-nbytes log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the stale-nbytes fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let committed = fs.lookup_path("/f").expect("the committed trees hold /f");
        assert_eq!(
            (committed.nlink, committed.size, committed.nbytes),
            (65, 33, 33),
            "/f is committed with 65 names and 33 bytes"
        );
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        let logged_f = top
            .inode(committed.ino)
            .unwrap()
            .expect("f's logged inode item");
        assert_eq!(
            (logged_f.generation, logged_f.nlink, logged_f.size),
            (committed.generation, 65, 33),
            "the log holds the committed f in full: {logged_f:?}"
        );
        assert!(
            logged_f.nbytes != 0 && logged_f.nbytes != committed.nbytes,
            "the log holds f with a counted nbytes, not the stale one: {logged_f:?}"
        );
        let extent = top
            .items
            .iter()
            .find(|i| {
                (i.key.objectid, i.key.key_type, i.key.offset)
                    == (committed.ino, fs_btrfs::fs::EXTENT_DATA_KEY, 0)
            })
            .unwrap_or_else(|| panic!("the log holds no extent of f at 0: {keys:?}"));
        assert!(
            extent.data.ends_with(REWRITTEN),
            "the logged extent of f holds the rewritten bytes inline: {:?}",
            extent.data
        );
        assert!(
            !top.items
                .iter()
                .any(|i| i.key.key_type == DIR_INDEX_KEY || i.key.key_type == DIR_LOG_INDEX_KEY),
            "only f was fsynced, so the log holds no entry of a directory: {keys:?}"
        );
    }
    let theirs = replays_as_the_kernel_does(NBYTES_FIXTURE, "dirtylog-nbytes");
    // The two replays agree; this is what they agree on, as the kernel
    // reports it, so a rewrite both left out cannot pass.
    let get = |kind: &str, name: &str| {
        theirs
            .get(&(kind.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("the kernel's replay reports no {kind} for {name}"))
    };
    for name in ["f".to_string(), long(0), long(63)] {
        assert_eq!(
            get("stat", &name),
            "f 65 33",
            "the kernel's replay of {name}"
        );
        assert_eq!(
            get("ino", &name),
            get("ino", "f"),
            "the kernel's replay names {name} as f"
        );
        assert_eq!(
            get("sha256", &name),
            sha256_hex(REWRITTEN),
            "{name} does not read back the rewritten bytes"
        );
    }
}

/// The committed file `f`, with 64 more names in `/` of 255 bytes each,
/// three of them in `INODE_EXTREF` items, given the name `g` in the
/// committed directory `d` and made durable by an fsync of `d` alone
/// (#476). The log holds the entry of `d` naming `g`, and `f` only as an
/// inode that exists: generation 0, 66 links, its `INODE_REF` items to
/// `/` and to `d`, and none of its extended references, which stay
/// committed. A replay that names `f` only from its `INODE_REF` items
/// counts 63 links; the kernel's replay of the same crash image keeps
/// all 66, and is the oracle.
#[test]
fn a_hard_link_of_a_file_with_extended_references_replays_as_the_kernel_does() {
    const EXTREF_EXISTS_FIXTURE: &str = "dirtylog/btrfs-dirty-log-extrefexists.img";
    /// `BTRFS_INODE_EXTREF_KEY`.
    const INODE_EXTREF_KEY: u8 = 13;
    let long = |i: usize| format!("{i:03}").repeat(85);
    let image = fixture(EXTREF_EXISTS_FIXTURE);
    let transcript =
        std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-extrefexists.logdump"))
            .expect("the native transcript of the extended-reference hard-link log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the extended-reference hard-link fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let committed = fs.lookup_path("/f").expect("the committed trees hold /f");
        assert_eq!(committed.nlink, 65, "/f is committed with 65 names");
        for i in [0, 61, 63] {
            let path = format!("/{}", long(i));
            let ino = fs
                .lookup_path(&path)
                .unwrap_or_else(|e| panic!("the committed trees should hold name {i}: {e}"))
                .ino;
            assert_eq!(ino, committed.ino, "committed name {i} is f");
        }
        let d = fs.lookup_path("/d").expect("the committed trees hold /d");
        assert!(
            fs.lookup_path("/d/g").is_err(),
            "/d/g was made after the commit, so the committed trees should not hold it"
        );
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        let logged_f = top
            .inode(committed.ino)
            .unwrap()
            .expect("f's logged inode item");
        assert_eq!(
            (logged_f.generation, logged_f.nlink),
            (0, 66),
            "the log holds f only as an inode that exists, with 66 links: {logged_f:?}"
        );
        assert!(
            !top.items.iter().any(|i| i.key.key_type == INODE_EXTREF_KEY),
            "the log holds no extended reference: {keys:?}"
        );
        assert!(
            !top.items.iter().any(|i| (i.key.objectid, i.key.key_type)
                == (committed.ino, fs_btrfs::fs::EXTENT_DATA_KEY)),
            "the log holds no data of f: {keys:?}"
        );
        // f's logged reference to d, read from the raw item: g's index in
        // d and the name.
        let reference = top
            .items
            .iter()
            .find(|i| {
                (i.key.objectid, i.key.key_type, i.key.offset)
                    == (committed.ino, INODE_REF_KEY, d.ino)
            })
            .unwrap_or_else(|| panic!("the log holds no reference of f to d: {keys:?}"));
        let r = &reference.data;
        let len = usize::from(u16::from_le_bytes(r[8..10].try_into().unwrap()));
        assert_eq!(
            (r.len(), &r[10..10 + len]),
            (10 + len, &b"g"[..]),
            "f's logged reference to d holds g alone"
        );
        let at_g = u64::from_le_bytes(r[..8].try_into().unwrap());
        let entry = top
            .items
            .iter()
            .find(|i| {
                (i.key.objectid, i.key.key_type, i.key.offset) == (d.ino, DIR_INDEX_KEY, at_g)
            })
            .unwrap_or_else(|| panic!("the log holds no entry of d at index {at_g}: {keys:?}"));
        assert_eq!(
            parse_dir_items(&entry.data)
                .unwrap()
                .iter()
                .map(|e| (e.ino, e.name.clone()))
                .collect::<Vec<_>>(),
            vec![(committed.ino, b"g".to_vec())],
            "the logged entry of d at index {at_g} names f by g"
        );
        assert!(
            top.items
                .iter()
                .filter(|i| i.key.objectid == d.ino && i.key.key_type == DIR_LOG_INDEX_KEY)
                .any(|i| i.key.offset <= at_g
                    && at_g <= u64::from_le_bytes(i.data[..8].try_into().unwrap())),
            "no logged index range of d holds g at {at_g}: {keys:?}"
        );
        assert!(
            !top.items.iter().any(|i| i.key.objectid == 256
                && (i.key.key_type == DIR_INDEX_KEY || i.key.key_type == DIR_LOG_INDEX_KEY)),
            "only d was fsynced, so the log holds no entry or index range of /: {keys:?}"
        );
    }
    let theirs = replays_as_the_kernel_does(EXTREF_EXISTS_FIXTURE, "dirtylog-extrefexists");
    // The two replays agree; this is what they agree on, as the kernel
    // reports it, so a name both left out, or a link count both got
    // wrong, cannot pass.
    let get = |kind: &str, name: &str| {
        theirs
            .get(&(kind.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("the kernel's replay reports no {kind} for {name}"))
    };
    for name in [
        "f".to_string(),
        long(0),
        long(61),
        long(63),
        "d/g".to_string(),
    ] {
        assert_eq!(
            get("stat", &name),
            "f 66 33",
            "the kernel's replay of {name}"
        );
        assert_eq!(
            get("ino", &name),
            get("ino", "f"),
            "the kernel's replay names {name} as f"
        );
        assert_eq!(
            get("sha256", &name),
            get("sha256", "f"),
            "{name} reads back differently from f"
        );
    }
}

/// `n`, created empty after the commit and given 64 more names in `/` of
/// 255 bytes each, three of them in `INODE_EXTREF` items, made durable by
/// an fsync of `/` alone (#477). The log holds the entries of `/` for all
/// 65 names, and `n` only as an inode that exists, though nothing of it
/// is committed: generation 0, size 0, 65 links, its `INODE_REF` to `/`
/// and its three extended references. A replay that names `n` only from
/// its `INODE_REF` counts 62 links; the kernel's replay of the same crash
/// image creates it with all 65, its extended references kept and the
/// generation of the transaction replaying it, and is the oracle.
#[test]
fn a_file_the_log_creates_with_extended_references_replays_as_the_kernel_does() {
    const EXTREF_CREATED_FIXTURE: &str = "dirtylog/btrfs-dirty-log-extrefcreated.img";
    /// `BTRFS_INODE_EXTREF_KEY`.
    const INODE_EXTREF_KEY: u8 = 13;
    let long = |i: usize| format!("{i:03}").repeat(85);
    let image = fixture(EXTREF_CREATED_FIXTURE);
    let transcript =
        std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-extrefcreated.logdump"))
            .expect("the native transcript of the created extended-reference log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    let (n, committed_generation) = {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the created extended-reference fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        assert!(
            fs.lookup_path("/n").is_err(),
            "n was made after the commit, so the committed trees should not hold it"
        );
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        let n = top.inode_named(256, b"n").expect("the log names n in /");
        assert!(
            fs.read_inode(n).is_err(),
            "the committed trees hold no inode {n}, so the log creates n"
        );
        let logged = top.inode(n).unwrap().expect("n's logged inode item");
        assert_eq!(
            (logged.generation, logged.size, logged.nlink),
            (0, 0, 65),
            "the log holds n only as an inode that exists, with 65 links: {logged:?}"
        );
        assert_eq!(
            top.items
                .iter()
                .filter(|i| (i.key.objectid, i.key.key_type) == (n, INODE_EXTREF_KEY))
                .count(),
            3,
            "the log holds three extended references of n: {keys:?}"
        );
        for i in [0, 60, 61, 62, 63] {
            assert_eq!(
                top.inode_named(256, long(i).as_bytes()),
                Some(n),
                "the log's references of n name {i} in /"
            );
        }
        assert!(
            !top.items
                .iter()
                .any(|i| (i.key.objectid, i.key.key_type) == (n, fs_btrfs::fs::EXTENT_DATA_KEY)),
            "the log holds no data of n: {keys:?}"
        );
        let entries: Vec<u64> = top
            .items
            .iter()
            .filter(|i| (i.key.objectid, i.key.key_type) == (256, DIR_INDEX_KEY))
            .map(|i| {
                let named = parse_dir_items(&i.data).unwrap();
                assert_eq!(
                    named.iter().map(|e| e.ino).collect::<Vec<_>>(),
                    vec![n],
                    "the logged entry of / at index {} names n",
                    i.key.offset
                );
                i.key.offset
            })
            .collect();
        assert_eq!(
            entries.len(),
            65,
            "the log holds an entry of / for every name of n"
        );
        for at in entries {
            assert!(
                top.items
                    .iter()
                    .filter(|i| i.key.objectid == 256 && i.key.key_type == DIR_LOG_INDEX_KEY)
                    .any(|i| i.key.offset <= at
                        && at <= u64::from_le_bytes(i.data[..8].try_into().unwrap())),
                "no logged index range of / holds index {at}: {keys:?}"
            );
        }
        (n, fs.superblock().generation)
    };
    // What the kernel's view does not show: n's inode item and its
    // extended references, as the native tool prints them from each
    // replayed copy.
    let inode_of_n = |image: &Path| {
        let dump = dump_tree(image, "5");
        let mut lines = dump
            .lines()
            .skip_while(|l| !l.contains(&format!("key ({n} INODE_ITEM 0) itemoff")));
        lines
            .next()
            .unwrap_or_else(|| panic!("{} holds no inode item of n", image.display()));
        let mut fields = BTreeMap::new();
        for line in lines.take(2) {
            let words: Vec<&str> = line.split_whitespace().collect();
            for name in ["generation", "size", "nbytes", "links"] {
                if let Some(at) = words.iter().position(|w| *w == name) {
                    fields.insert(name, words[at + 1].to_string());
                }
            }
        }
        let extrefs: Vec<String> = dump
            .lines()
            .filter_map(|l| {
                let (_, rest) = l.split_once(&format!("key ({n} INODE_EXTREF "))?;
                let (hash, rest) = rest.split_once(')')?;
                rest.contains("itemoff").then(|| hash.to_string())
            })
            .collect();
        (fields, extrefs)
    };
    let theirs = replays_as_the_kernel_does_then(
        EXTREF_CREATED_FIXTURE,
        "dirtylog-extrefcreated",
        |theirs, ours| {
            let (kernel, kernel_extrefs) = inode_of_n(theirs);
            let (this, this_extrefs) = inode_of_n(ours);
            let replayed_in = (committed_generation + 1).to_string();
            assert_eq!(
                [
                    kernel.get("generation"),
                    kernel.get("size"),
                    kernel.get("nbytes"),
                    kernel.get("links")
                ],
                [
                    Some(&replayed_in),
                    Some(&"0".to_string()),
                    Some(&"0".to_string()),
                    Some(&"65".to_string())
                ],
                "the kernel's replay creates n in its own transaction, empty, with 65 links"
            );
            assert_eq!(
                this, kernel,
                "n's inode item, this crate's replay and the kernel's"
            );
            assert_eq!(
                kernel_extrefs.len(),
                3,
                "the kernel's replay keeps n's three extended references: {kernel_extrefs:?}"
            );
            assert_eq!(
                this_extrefs, kernel_extrefs,
                "n's extended references, this crate's replay and the kernel's"
            );
        },
    );
    let get = |kind: &str, name: &str| {
        theirs
            .get(&(kind.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("the kernel's replay reports no {kind} for {name}"))
    };
    for name in ["n".to_string(), long(0), long(60), long(61), long(63)] {
        assert_eq!(
            get("stat", &name),
            "f 65 0",
            "the kernel's replay of {name}"
        );
        assert_eq!(
            get("ino", &name),
            get("ino", "n"),
            "the kernel's replay names {name} as n"
        );
    }
}

/// `f`, committed with one link, its only name the last of 64 long names
/// in `/`, held in an `INODE_EXTREF` item once its `INODE_REF` was
/// emptied, loses that name after the commit, and only `/` is fsynced
/// (#478). The log holds an index range of `/` covering the removed index
/// and no entry there, and does not hold `f`. A replay that looks for the
/// name only in `INODE_REF` items refuses the log; the kernel's replay of
/// the same crash image deletes `f` with its extended reference and its
/// data, and is the oracle.
#[test]
fn an_inode_whose_last_name_is_an_extended_reference_is_deleted_as_the_kernel_does() {
    const EXTREF_REMOVED_FIXTURE: &str = "dirtylog/btrfs-dirty-log-extrefremoved.img";
    let last = "063".repeat(85);
    let image = fixture(EXTREF_REMOVED_FIXTURE);
    let transcript =
        std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-extrefremoved.logdump"))
            .expect("the native transcript of the removed extended-reference log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    // f and the index of its last name, as the native tool prints the
    // committed trees: one INODE_EXTREF item, whose record reads
    // `index N parent P namelen L name: X`.
    let committed = dump_tree(&image, "5");
    let mut extrefs: Vec<(u64, u64, u64, String)> = Vec::new();
    let mut lines = committed.lines();
    while let Some(line) = lines.next() {
        let Some((_, key)) = line.split_once("key (") else {
            continue;
        };
        let words: Vec<&str> = key.split_whitespace().collect();
        if words.get(1) != Some(&"INODE_EXTREF") {
            continue;
        }
        let record: Vec<&str> = lines
            .next()
            .expect("an extended reference holds a record")
            .split_whitespace()
            .collect();
        assert_eq!(
            [record[0], record[2], record[6]],
            ["index", "parent", "name:"],
            "a record of an extended reference, as the native tool prints it: {record:?}"
        );
        extrefs.push((
            words[0].parse().unwrap(),
            record[1].parse().unwrap(),
            record[3].parse().unwrap(),
            record[7].to_string(),
        ));
    }
    assert_eq!(
        extrefs.len(),
        1,
        "the committed trees hold one extended reference: {extrefs:?}"
    );
    let (f, at, parent, name) = extrefs.pop().unwrap();
    assert_eq!(
        (parent, name.as_str()),
        (256, last.as_str()),
        "f's extended reference names its last long name in /"
    );
    {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the removed extended-reference fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        assert!(
            fs.lookup_path("/f").is_err(),
            "the name f was removed before the commit"
        );
        let committed_f = fs
            .lookup_path(&format!("/{last}"))
            .expect("the committed trees hold the last long name");
        assert_eq!(
            (committed_f.ino, committed_f.nlink),
            (f, 1),
            "the last long name is f, committed with one link"
        );
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        assert!(
            !top.items.iter().any(|i| i.key.objectid == f),
            "the log holds no item of f: {keys:?}"
        );
        for entry in top
            .items
            .iter()
            .filter(|i| (i.key.objectid, i.key.key_type) == (256, DIR_INDEX_KEY))
        {
            assert_ne!(
                entry.key.offset, at,
                "the log holds an entry of / at the removed index: {keys:?}"
            );
            assert!(
                parse_dir_items(&entry.data)
                    .unwrap()
                    .iter()
                    .all(|e| e.ino != f),
                "the logged entry of / at index {} names f",
                entry.key.offset
            );
        }
        assert!(
            top.items
                .iter()
                .filter(|i| i.key.objectid == 256 && i.key.key_type == DIR_LOG_INDEX_KEY)
                .any(|i| i.key.offset <= at
                    && at <= u64::from_le_bytes(i.data[..8].try_into().unwrap())),
            "no logged index range of / holds the removed index {at}: {keys:?}"
        );
    }
    // What the kernel's view does not show: every item of f, and every
    // entry naming it, as the native tool prints each replayed copy.
    let items_of_f = |image: &Path| -> Vec<String> {
        dump_tree(image, "5")
            .lines()
            .filter(|l| l.contains(&format!("key ({f} ")))
            .map(|l| l.trim().to_string())
            .collect()
    };
    let theirs = replays_as_the_kernel_does_then(
        EXTREF_REMOVED_FIXTURE,
        "dirtylog-extrefremoved",
        |theirs, ours| {
            assert_eq!(
                items_of_f(theirs),
                Vec::<String>::new(),
                "the kernel's replay deletes f with its extended reference and its data"
            );
            assert_eq!(
                items_of_f(ours),
                Vec::<String>::new(),
                "this crate's replay deletes f with its extended reference and its data"
            );
        },
    );
    assert_eq!(
        theirs.get(&("stat".to_string(), last.clone())),
        None,
        "the kernel's replay removes the last name of f"
    );
    assert_eq!(
        theirs
            .get(&("stat".to_string(), "k".to_string()))
            .map(String::as_str),
        Some("f 1 4"),
        "the kernel's replay keeps k"
    );
}

/// `e`, a committed empty directory in `/`, is removed after the commit,
/// and only `/` is fsynced (#410). The log holds an index range of `/`
/// covering the removed index and no entry there, and does not hold `e`.
/// A replay that removes only regular files refuses the log; the kernel's
/// replay of the same crash image removes the directory with its items,
/// and is the oracle.
#[test]
fn a_directory_whose_name_the_log_removes_is_removed_as_the_kernel_does() {
    const RMDIR_FIXTURE: &str = "dirtylog/btrfs-dirty-log-rmdir.img";
    let image = fixture(RMDIR_FIXTURE);
    let transcript = std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-rmdir.logdump"))
        .expect("the native transcript of the removed directory log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    let e = {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the removed directory fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let committed_e = fs
            .lookup_path("/e")
            .expect("the committed trees hold the directory e");
        assert!(
            fs.read_dir(committed_e.ino)
                .expect("e is a committed directory")
                .is_empty(),
            "e is committed empty"
        );
        let e = committed_e.ino;
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        assert!(
            !top.items.iter().any(|i| i.key.objectid == e),
            "the log holds no item of e: {keys:?}"
        );
        for entry in top
            .items
            .iter()
            .filter(|i| (i.key.objectid, i.key.key_type) == (256, DIR_INDEX_KEY))
        {
            assert!(
                parse_dir_items(&entry.data)
                    .unwrap()
                    .iter()
                    .all(|d| d.ino != e),
                "the logged entry of / at index {} names e",
                entry.key.offset
            );
        }
        assert!(
            top.items
                .iter()
                .any(|i| i.key.objectid == 256 && i.key.key_type == DIR_LOG_INDEX_KEY),
            "the log holds no index range of /: {keys:?}"
        );
        e
    };
    // What the kernel's view does not show: every item of e, and every
    // entry naming it, as the native tool prints each replayed copy.
    let items_of_e = |image: &Path| -> Vec<String> {
        dump_tree(image, "5")
            .lines()
            .filter(|l| {
                l.contains(&format!("key ({e} ")) || l.contains(&format!("location key ({e} "))
            })
            .map(|l| l.trim().to_string())
            .collect()
    };
    let theirs =
        replays_as_the_kernel_does_then(RMDIR_FIXTURE, "dirtylog-rmdir", |theirs, ours| {
            assert_eq!(
                items_of_e(theirs),
                Vec::<String>::new(),
                "the kernel's replay removes e with its items"
            );
            assert_eq!(
                items_of_e(ours),
                Vec::<String>::new(),
                "this crate's replay removes e with its items"
            );
        });
    assert_eq!(
        theirs.get(&("stat".to_string(), "e".to_string())),
        None,
        "the kernel's replay removes the name e"
    );
    assert_eq!(
        theirs
            .get(&("stat".to_string(), "k".to_string()))
            .map(String::as_str),
        Some("f 1 4"),
        "the kernel's replay keeps k"
    );
}

/// `f` and `g`, two committed names of one file in `/`, lose `g` after
/// the commit, and only `/` is fsynced (#411). The log holds an index
/// range of `/` covering the removed index and no entry there, and does
/// not hold the file. A replay that removes only an inode's last name
/// refuses the log; the kernel's replay of the same crash image drops the
/// name `g` and its reference record and leaves the file with one link,
/// and is the oracle.
#[test]
fn a_name_whose_inode_keeps_another_is_removed_as_the_kernel_does() {
    const LINK_REMOVED_FIXTURE: &str = "dirtylog/btrfs-dirty-log-linkremoved.img";
    let image = fixture(LINK_REMOVED_FIXTURE);
    let transcript =
        std::fs::read_to_string(fixture("dirtylog/btrfs-dirty-log-linkremoved.logdump"))
            .expect("the native transcript of the removed name log");
    transcript_binding(&transcript, &std::fs::read(&image).unwrap())
        .expect("the transcript is of this exact image");
    let f = {
        let fs = Filesystem::mount_ignoring_log(Arc::new(
            FileDevice::open(&image).expect("open the removed name fixture"),
        ) as Arc<dyn BlockRead>)
        .expect("read the committed trees without replay");
        let committed_f = fs
            .lookup_path("/f")
            .expect("the committed trees hold the file f");
        let committed_g = fs
            .lookup_path("/g")
            .expect("the committed trees hold the name g");
        assert_eq!(committed_f.ino, committed_g.ino, "f and g are one inode");
        assert_eq!(committed_f.nlink, 2, "f is committed with two links");
        let f = committed_f.ino;
        let log = fs.log().unwrap().expect("a dirty log");
        let top: &LoggedTree = log
            .trees
            .iter()
            .find(|t| t.subvolume == 5)
            .unwrap_or_else(|| panic!("no log tree for the top-level subvolume: {log:?}"));
        let keys: Vec<String> = top
            .items
            .iter()
            .map(|i| format!("({} {} {})", i.key.objectid, i.key.key_type, i.key.offset))
            .collect();
        assert!(
            !top.items.iter().any(|i| i.key.objectid == f),
            "the log holds no item of f: {keys:?}"
        );
        for entry in top
            .items
            .iter()
            .filter(|i| (i.key.objectid, i.key.key_type) == (256, DIR_INDEX_KEY))
        {
            assert!(
                parse_dir_items(&entry.data)
                    .unwrap()
                    .iter()
                    .all(|d| d.ino != f),
                "the logged entry of / at index {} names f",
                entry.key.offset
            );
        }
        assert!(
            top.items
                .iter()
                .any(|i| i.key.objectid == 256 && i.key.key_type == DIR_LOG_INDEX_KEY),
            "the log holds no index range of /: {keys:?}"
        );
        f
    };
    // What the kernel's view does not show: the file's reference records,
    // as the native tool prints each replayed copy.
    let refs_of_f = |image: &Path| -> Vec<String> {
        let dump = dump_tree(image, "5");
        let mut lines = dump.lines().map(str::trim);
        let mut refs = Vec::new();
        while let Some(line) = lines.next() {
            if line.contains(&format!("key ({f} INODE_REF ")) {
                refs.push(line.split(" itemoff").next().unwrap_or(line).to_string());
                refs.extend(
                    lines
                        .clone()
                        .take_while(|l| l.starts_with("index "))
                        .map(str::to_string),
                );
            }
        }
        refs
    };
    let theirs = replays_as_the_kernel_does_then(
        LINK_REMOVED_FIXTURE,
        "dirtylog-linkremoved",
        |theirs, ours| {
            let kernel = refs_of_f(theirs);
            assert_eq!(
                kernel.len(),
                2,
                "the kernel's replay keeps one INODE_REF with one record: {kernel:?}"
            );
            assert!(
                kernel[1].ends_with("name: f"),
                "the kernel's replay keeps the name f: {kernel:?}"
            );
            assert_eq!(
                refs_of_f(ours),
                kernel,
                "this crate's replay keeps the reference records the kernel's does"
            );
        },
    );
    assert_eq!(
        theirs.get(&("stat".to_string(), "g".to_string())),
        None,
        "the kernel's replay removes the name g"
    );
    assert_eq!(
        theirs
            .get(&("stat".to_string(), "f".to_string()))
            .map(String::as_str),
        Some("f 1 4"),
        "the kernel's replay keeps f with one link"
    );
}

/// The inline writer's encoding fields are checked against the native
/// tools, not against our own parser. `dump-tree` prints an inline file
/// extent's data size, `ram_bytes` and compression; the raw item must hold
/// exactly those at the public format's offsets (`ram_bytes` at 8,
/// compression at 16, inline data from 21). The native tool does not print
/// encryption or other_encoding, so those two fields stay unattested here.
#[test]
fn the_logged_inline_extent_fields_match_the_native_transcript() {
    let transcript = std::fs::read_to_string(fixture(INLINE_TRANSCRIPT))
        .expect("the native transcript of the inline overwrite log");
    inline_transcript_binding(
        &transcript,
        &std::fs::read(fixture(INLINE_FIXTURE)).unwrap(),
    )
    .expect("the transcript is of this exact image");
    let ino = Filesystem::mount_ignoring_log(Arc::new(
        FileDevice::open(fixture(INLINE_FIXTURE)).unwrap(),
    ) as Arc<dyn BlockRead>)
    .unwrap()
    .lookup_path("/inline-overwrite")
    .unwrap()
    .ino;
    // The addressed record: the item header for this inode's extent at
    // offset 0, then the first inline-extent line inside that item.
    let header = format!("key ({ino} EXTENT_DATA 0)");
    let line = transcript
        .lines()
        .skip_while(|l| !l.contains(&header))
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with("item "))
        .find(|l| l.contains("inline extent data size"))
        .unwrap_or_else(|| panic!("the native dump prints no inline extent for {header}"));
    let field = |name: &str| -> u64 {
        let rest = line
            .split(name)
            .nth(1)
            .unwrap_or_else(|| panic!("no {name} in {line}"));
        rest.split_whitespace().next().unwrap().parse().unwrap()
    };
    let (size, ram, compression) = (
        field("inline extent data size "),
        field("ram_bytes "),
        field("compression "),
    );
    let fs = Filesystem::mount_ignoring_log(Arc::new(
        FileDevice::open(fixture(INLINE_FIXTURE)).unwrap(),
    ) as Arc<dyn BlockRead>)
    .unwrap();
    let log = fs.log().unwrap().expect("a dirty inline overwrite log");
    let item = log.trees[0]
        .items
        .iter()
        .find(|i| {
            (i.key.objectid, i.key.key_type, i.key.offset)
                == (ino, fs_btrfs::fs::EXTENT_DATA_KEY, 0)
        })
        .expect("the logged file extent");
    let raw = &item.data;
    assert_eq!(raw[20], 0, "the logged extent is inline");
    assert_eq!(u64::from_le_bytes(raw[8..16].try_into().unwrap()), ram);
    assert_eq!(u64::from(raw[16]), compression);
    assert_eq!((raw.len() - 21) as u64, size);
    assert_eq!(
        compression, 0,
        "the fixture's inline extent is uncompressed"
    );
    assert_eq!(ram, size, "an uncompressed inline extent holds ram_bytes");
}
