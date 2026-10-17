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

use fs_btrfs::fs::Filesystem;
use fs_btrfs::inode::{INODE_ITEM_KEY, INODE_REF_KEY};
use fs_btrfs::log_tree::LoggedTree;
use fs_btrfs::Error;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, fixture, guest_kernel_read_ok, guest_kernel_report,
    guest_kernel_write_ok, oracle, temp_path,
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

/// Every path's type, link count and size, and every regular file's
/// modification time, as the kernel reports them for the mounted image.
///
/// A directory's times are left out: a name the replay removes moves its
/// directory's times to the moment of the replay, in the kernel's replay
/// as in this crate's, and the two replays happen at different moments.
const STATS: &str = r#"
cd "$MNT"
find . -mindepth 1 -printf 'stat\t%P\t%y %n %s\n' | sort
find . -mindepth 1 -type f -printf 'mtime\t%P\t%T@\n' | sort
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
fn replays_as_the_kernel_does(fixture_name: &str, tag: &str) {
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
    let _ = std::fs::remove_file(&theirs);
    let _ = std::fs::remove_file(&ours);
    let differ = differences(&ours_view, &theirs_view);
    assert!(
        differ.is_empty(),
        "[{tag}] the volume this crate replayed differs from the kernel's replay:\n{}",
        differ.join("\n")
    );
}

/// `durable.txt` rewritten in place and `fsynced.txt` created, both made
/// durable by fsync alone (#376): the new file exists only in the log, as
/// its inode item, its reference to `/` and its inline extent, with no
/// directory item for it. The kernel's replay is the oracle.
#[test]
fn a_read_write_mount_replays_the_log_as_the_kernel_does() {
    replays_as_the_kernel_does(DIRTY_LOG_FIXTURE, "dirtylog");
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
fn an_unsupported_log_is_refused_without_writing(shape: &str) {
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

/// A log of shapes replay does not take yet -- data appended, truncated
/// and partly overwritten, names removed and renamed (#374) -- is refused
/// before anything is written.
#[test]
fn a_log_replay_cannot_take_yet_is_refused_without_writing() {
    an_unsupported_log_is_refused_without_writing("replay");
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

/// An inline extent that grows from 33 bytes to 43 (#378): the file's
/// size and `nbytes` move with it.
#[test]
fn a_size_changing_inline_rewrite_replays_as_the_kernel_does() {
    an_inline_rewrite_replays_as_the_kernel_does(
        "sizechange",
        b"replayed-and-longer-inline-bytes-0123456789",
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
