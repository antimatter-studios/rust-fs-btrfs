//! Shared helpers for the btrfs test suite: where scratch files live,
//! where fixtures come from, the oracle tools (see [`oracle`]) and the
//! kernel oracles (see [`guest_kernel_report`]).
//!
//! ONE WAY IN, FOR EACH OF THE THREE THINGS A TEST CAN REACH FOR — a
//! fixture, a btrfs-progs tool, the kernel — and none of them has an
//! early return. That is what makes `scripts/test-targets.sh` able to
//! sort the suite into tiers by reading the sources, and what makes
//! `tests/test_contract.rs` able to refuse every other shape.

mod kernel;
mod oracle;

pub use kernel::{
    guest_kernel_pool_check, guest_kernel_pool_read, guest_kernel_pool_write, guest_kernel_probe,
    guest_kernel_read, guest_kernel_read_ok, guest_kernel_read_variants, guest_kernel_report,
    guest_kernel_write, guest_kernel_write_ok, KernelProbe, PoolRead, Variant, VariantVerdict,
};
pub use oracle::{guest_base64, guest_quote, oracle, Oracle, Run};

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_TEMP_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The scratch root: `<repo>/tmp`, or `FS_BTRFS_TEST_TMPDIR` when a
/// caller supplies an exact directory of its own.
///
/// ONE RULE, AND IT IS THE ORACLE'S. Scratch files are what the oracle
/// tools read, and those tools run in the harness VM, which sees this
/// repository mounted at the path the host knows it by — and nothing
/// else of the host. A scratch directory under `/tmp` or `$RUNNER_TEMP`
/// would not exist there. So it lives in the repository (gitignored),
/// on every machine and on CI alike, and a caller-supplied directory
/// outside the repository is refused rather than quietly breaking every
/// oracle test.
#[track_caller]
pub fn select_temp_dir(explicit: Option<&OsStr>, worktree: &Path) -> PathBuf {
    let Some(path) = explicit.filter(|path| !path.is_empty()) else {
        return worktree.join("tmp");
    };
    let path = PathBuf::from(path);
    assert!(
        path.starts_with(worktree),
        "FS_BTRFS_TEST_TMPDIR is {}, which is outside {}. The oracle tools run in the \
         harness VM, which sees this repository and nothing else of the host, so scratch \
         files have to live inside it.",
        path.display(),
        worktree.display()
    );
    path
}

/// Create a collision-resistant per-process scratch directory below `base`.
#[doc(hidden)]
pub fn create_unique_temp_dir(base: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(base)?;
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..1_024_u16 {
        let candidate = base.join(format!(
            "fs-btrfs-tests.{}.{}.{}",
            std::process::id(),
            started,
            attempt
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "cannot allocate unique scratch directory below {}",
            base.display()
        ),
    ))
}

/// Preserve an explicit directory or isolate a process beneath a selected root.
#[doc(hidden)]
pub fn materialize_temp_dir(explicit: Option<&OsStr>, root: &Path) -> io::Result<PathBuf> {
    if explicit.filter(|path| !path.is_empty()).is_some() {
        fs::create_dir_all(root)?;
        Ok(root.to_path_buf())
    } else {
        create_unique_temp_dir(root)
    }
}

/// This repository's root.
pub fn repo_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        // <repo>/tests/support -> <repo>
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the test support crate lives at <repo>/tests/support")
            .to_path_buf()
    })
}

/// Return the shared scratch directory for this integration-test process.
pub fn temp_dir() -> &'static Path {
    TEST_TEMP_DIR
        .get_or_init(|| {
            let worktree = repo_root();
            let explicit = std::env::var_os("FS_BTRFS_TEST_TMPDIR");
            let selected_root = select_temp_dir(explicit.as_deref(), worktree);
            materialize_temp_dir(explicit.as_deref(), &selected_root).unwrap_or_else(|error| {
                panic!(
                    "cannot create btrfs test scratch directory below {}: {error}",
                    selected_root.display()
                )
            })
        })
        .as_path()
}

/// `text` with `dir`'s path shown as `$TMP` wherever it appears (#635).
/// Every evidence line names files in the scratch directory, whose full
/// path, about 70 bytes on CI, would otherwise repeat on each of them.
pub fn shorten_with(text: &str, dir: &Path) -> String {
    let full = dir.to_string_lossy();
    if full.is_empty() {
        return text.to_string();
    }
    text.replace(full.as_ref(), "$TMP")
}

/// One evidence line as it is printed: the scratch directory `dir` shown
/// as `$TMP`, and, when `header` is asked for and the line names `dir`, an
/// `[evidence] $TMP = <full path>` line before it, so the file a failing
/// call read can still be found. `dir` is `None` in a process that made no
/// scratch directory; a line naming none prints unchanged (#635).
pub fn evidence_text(line: &str, dir: Option<&Path>, header: bool) -> String {
    let Some(dir) = dir.filter(|dir| {
        let full = dir.to_string_lossy();
        !full.is_empty() && line.contains(full.as_ref())
    }) else {
        return line.to_string();
    };
    let short = shorten_with(line, dir);
    if header {
        format!("[evidence] $TMP = {}\n{short}", dir.display())
    } else {
        short
    }
}

/// Print an evidence line through [`evidence_text`], with the header the
/// first time a line in this process names its scratch directory (#635).
pub fn evidence(line: &str) {
    static HEADER: AtomicBool = AtomicBool::new(false);
    let dir = TEST_TEMP_DIR.get().map(PathBuf::as_path);
    let names_it = dir.is_some_and(|dir| line.contains(dir.to_string_lossy().as_ref()));
    let header = names_it && !HEADER.swap(true, Ordering::SeqCst);
    println!("{}", evidence_text(line, dir, header));
}

/// Format a test filename beneath the selected scratch directory.
#[doc(hidden)]
pub fn formatted_temp_path(arguments: fmt::Arguments<'_>) -> String {
    temp_dir()
        .join(arguments.to_string())
        .to_string_lossy()
        .into_owned()
}

#[macro_export]
macro_rules! temp_path {
    ($($argument:tt)*) => {
        $crate::formatted_temp_path(format_args!($($argument)*))
    };
}

/// Copy `from` to `to`, leaving a hole wherever `from` reads as zeros.
///
/// `std::fs::copy` on Linux writes a sparse file out in full: a fixture holding
/// 36 MiB takes all of its 512 MiB in the copy (#731, #747). A copy reads the
/// same bytes as its source, which is all a checker or a mount can see.
/// `tests/scripts/fixture-copies-sparse.sh` holds the suite to this.
pub fn copy_sparse(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    const BLOCK: usize = 4096;
    let mut source = fs::File::open(from)?;
    let target = fs::File::create(to)?;
    target.set_len(source.metadata()?.len())?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut at = 0u64;
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        for (index, block) in buffer[..read].chunks(BLOCK).enumerate() {
            if block.iter().any(|&byte| byte != 0) {
                target.write_all_at(block, at + (index * BLOCK) as u64)?;
            }
        }
        at += read as u64;
    }
}

/// A disk-backed, sparse copy of an image at one point in a test. The copy is
/// removed when the snapshot drops, including when an assertion panics.
pub struct ImageSnapshot(PathBuf);

impl ImageSnapshot {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ImageSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Preserve an image without keeping its bytes in the test process's RAM.
pub fn snapshot_image(image: impl AsRef<Path>) -> io::Result<ImageSnapshot> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "image-snapshot-{}-{}.img",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = temp_dir().join(name);
    copy_sparse(image, &path)?;
    Ok(ImageSnapshot(path))
}

/// The first byte where two images differ, or their differing lengths.
#[derive(Debug, PartialEq, Eq)]
pub enum ImageDifference {
    Length { left: u64, right: u64 },
    Byte { offset: u64, left: u8, right: u8 },
}

impl fmt::Display for ImageDifference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { left, right } => {
                write!(f, "lengths differ: {left} versus {right} bytes")
            }
            Self::Byte {
                offset,
                left,
                right,
            } => write!(
                f,
                "first differing byte at 0x{offset:x}: 0x{left:02x} versus 0x{right:02x}"
            ),
        }
    }
}

/// Compare two images with two 1 MiB buffers, independent of image size.
pub fn first_image_difference(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
) -> io::Result<Option<ImageDifference>> {
    let mut left_file = fs::File::open(left)?;
    let mut right_file = fs::File::open(right)?;
    let left_len = left_file.metadata()?.len();
    let right_len = right_file.metadata()?.len();
    if left_len != right_len {
        return Ok(Some(ImageDifference::Length {
            left: left_len,
            right: right_len,
        }));
    }
    let mut left_bytes = vec![0u8; 1 << 20];
    let mut right_bytes = vec![0u8; 1 << 20];
    let mut offset = 0u64;
    while offset < left_len {
        let n = (left_len - offset).min(left_bytes.len() as u64) as usize;
        left_file.read_exact(&mut left_bytes[..n])?;
        right_file.read_exact(&mut right_bytes[..n])?;
        if let Some(index) = left_bytes[..n]
            .iter()
            .zip(&right_bytes[..n])
            .position(|(a, b)| a != b)
        {
            return Ok(Some(ImageDifference::Byte {
                offset: offset + index as u64,
                left: left_bytes[index],
                right: right_bytes[index],
            }));
        }
        offset += n as u64;
    }
    Ok(None)
}

#[track_caller]
pub fn assert_images_equal(left: impl AsRef<Path>, right: impl AsRef<Path>, context: &str) {
    match first_image_difference(left, right).expect("compare the images") {
        None => {}
        Some(difference) => panic!("{context}: {difference}"),
    }
}

#[track_caller]
pub fn assert_images_differ(left: impl AsRef<Path>, right: impl AsRef<Path>, context: &str) {
    assert!(
        first_image_difference(left, right)
            .expect("compare the images")
            .is_some(),
        "{context}: the images are identical"
    );
}

#[track_caller]
pub fn assert_image_unchanged(image: impl AsRef<Path>, before: &ImageSnapshot, context: &str) {
    assert_images_equal(image, before.path(), context);
}

/// Compare a file with bytes already produced by a command, without loading
/// the file into a second Vec.
#[track_caller]
pub fn assert_file_bytes(path: impl AsRef<Path>, expected: &[u8], context: &str) {
    let mut file = fs::File::open(path).expect("open compared file");
    let len = file.metadata().expect("stat compared file").len();
    assert_eq!(len, expected.len() as u64, "{context}: file length differs");
    let mut buffer = vec![0u8; 1 << 20];
    for (part, chunk) in expected.chunks(buffer.len()).enumerate() {
        file.read_exact(&mut buffer[..chunk.len()])
            .expect("read compared file");
        if let Some(index) = buffer[..chunk.len()]
            .iter()
            .zip(chunk)
            .position(|(a, b)| a != b)
        {
            let offset = part * buffer.len() + index;
            panic!(
                "{context}: first differing byte at 0x{offset:x}: file 0x{:02x}, expected 0x{:02x}",
                buffer[index], chunk[index]
            );
        }
    }
}

/// Hash a file through a 1 MiB buffer. This is for digest checks that do
/// not need to retain a second copy of a disk image.
pub fn sha256_file(path: impl AsRef<Path>) -> io::Result<String> {
    use sha2::Digest;
    let mut file = fs::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&buffer[..n]);
    }
}

/// Read a genuinely small file in full. The limit also catches a test that
/// accidentally passes a disk image through a small-output assertion.
pub fn read_small_file(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    read_bounded_file(path, 1 << 20)
}

/// Read non-image command output with an explicit upper bound.
pub fn read_bounded_file(path: impl AsRef<Path>, max: u64) -> io::Result<Vec<u8>> {
    let max = max.min(16 << 20);
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds the {max} byte whole-read limit"),
        ));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file grew beyond the {max} byte whole-read limit"),
        ));
    }
    Ok(bytes)
}

/// Read one bounded region of a disk image, for superblocks and tree nodes.
pub fn read_image_region(path: impl AsRef<Path>, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    if len > 1 << 20 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "image region exceeds 1 MiB",
        ));
    }
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Write just one edited region, preserving the rest of a sparse image.
pub fn write_image_region(path: impl AsRef<Path>, offset: u64, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)
}

/// Visit fixed-size windows in place. A window is written only when `edit`
/// returns true, so scanning a sparse image does not fill its holes.
pub fn edit_image_windows(
    path: impl AsRef<Path>,
    window: usize,
    step: usize,
    mut edit: impl FnMut(u64, &mut [u8]) -> bool,
) -> io::Result<usize> {
    if window == 0 || window > 1 << 20 || step == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid image window",
        ));
    }
    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let size = file.metadata()?.len();
    let mut bytes = vec![0; window];
    let mut changed = 0;
    let mut offset = 0u64;
    while offset + window as u64 <= size {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        if edit(offset, &mut bytes) {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&bytes)?;
            changed += 1;
        }
        offset += step as u64;
    }
    Ok(changed)
}

/// Count a pattern at 512-byte sector starts without retaining the image.
pub fn count_image_sector_matches(path: impl AsRef<Path>, needle: &[u8]) -> io::Result<usize> {
    if needle.is_empty() || needle.len() > 512 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid sector pattern",
        ));
    }
    let mut file = fs::File::open(path)?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut count = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(count);
        }
        count += buffer[..n]
            .chunks_exact(512)
            .filter(|sector| &sector[..needle.len()] == needle)
            .count();
    }
}

#[cfg(test)]
mod image_compare_tests {
    use super::*;

    #[test]
    fn sparse_snapshot_and_comparison_report_the_first_offset() {
        let image = temp_dir().join("comparison-source.img");
        fs::File::create(&image).unwrap().set_len(4 << 20).unwrap();
        write_image_region(&image, (1 << 20) + 3, &[0x11]).unwrap();
        let before = snapshot_image(&image).unwrap();
        assert_images_equal(&image, before.path(), "snapshot starts equal");

        write_image_region(&image, (1 << 20) + 3, &[0x22]).unwrap();
        assert_eq!(
            first_image_difference(&image, before.path()).unwrap(),
            Some(ImageDifference::Byte {
                offset: (1 << 20) + 3,
                left: 0x22,
                right: 0x11,
            })
        );
        fs::File::options()
            .write(true)
            .open(&image)
            .unwrap()
            .set_len(5 << 20)
            .unwrap();
        assert_eq!(
            first_image_difference(&image, before.path()).unwrap(),
            Some(ImageDifference::Length {
                left: 5 << 20,
                right: 4 << 20,
            })
        );
        assert!(read_small_file(&image).is_err());
    }
}

/// Where the generated fixtures live: `<repo>/test-disks`.
///
/// NOT the harness's share directory. `.vm-share` is what a run hands
/// across to the guest and back; the fixtures are inputs to the suite,
/// they are named in `chore fixtures`' `generates:`, and CI passes them
/// between jobs as an artifact. Keeping them apart is what stops a
/// scratch image a test left in the share from being picked up as a
/// fixture by the next suite that walks the directory — which is a
/// failure this repository has had.
pub fn fixture_dir() -> PathBuf {
    repo_root().join("test-disks")
}

/// The path of a generated fixture under `test-disks/`, or a panic that
/// says how to build it.
///
/// The images are gitignored and built by `chore fixtures` (the kernel
/// populates them, inside the fs-linux-test-harness VM). THE ONLY WAY A
/// TEST REACHES A FIXTURE: a test that found its image absent used to
/// print "skip" and return, and a skipped test reads exactly like a
/// passing one, so a checkout without fixtures ran most of this suite
/// against nothing and reported green — which is how the leaf oracle
/// went unnoticed running against no fixtures at all. `chore test:unit`
/// also relies on this: a test binary that never calls it needs no
/// fixture.
#[track_caller]
pub fn fixture(name: &str) -> PathBuf {
    let path = fixture_dir().join(name);
    assert!(
        path.is_file(),
        "test-disks/{name} is missing: the fixtures are gitignored and generated. \
         Build them with `chore fixtures` (it boots the fs-linux-test-harness VM; \
         `chore siblings` checks the harness out) and run the tests again. \
         Tests never skip on a missing fixture."
    );
    path
}

/// Every `test-disks/btrfs-*.img` matching `prefix`, sorted, and never
/// an empty list.
///
/// The suites that walk "every fixture" are the ones that used to print
/// `no fixtures — skipping` and return 0, so the emptiness check is the
/// whole point of this helper rather than a nicety.
#[track_caller]
pub fn fixtures_matching(prefix: &str) -> Vec<PathBuf> {
    let dir = fixture_dir();
    let mut found: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|error| {
            panic!(
                "cannot read {}: {error}. The fixtures are built by `chore fixtures`.",
                dir.display()
            )
        })
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|e| e == "img")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    found.sort();
    assert!(
        !found.is_empty(),
        "no {prefix}*.img fixtures in {}: the fixtures are gitignored and generated. \
         Build them with `chore fixtures`. Tests never skip on a missing fixture.",
        dir.display()
    );
    found
}

/// `btrfs check --readonly` on `image` must exit 0, or the test fails
/// with the checker's report.
///
/// This suite's oracles compared their images with this crate's own
/// reader, which cannot see a wrong checksum or an unreachable extent.
/// `btrfs check` is the reference consistency checker, and `--readonly`
/// answers no to every repair, so it reports without touching the
/// image. It runs in the harness VM, like every oracle tool (see
/// [`oracle`]).
#[track_caller]
pub fn assert_btrfs_check_clean(image: &Path, tag: &str) {
    let out = oracle("btrfs")
        .args(["check", "--readonly"])
        .arg(image)
        .output();
    assert_eq!(
        out.status.code(),
        Some(0),
        "[{tag}] btrfs check --readonly {}:\n{}{}",
        image.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `btrfs inspect-internal dump-super -f` on `image`, in the guest.
#[track_caller]
pub fn dump_super(image: &Path) -> String {
    let out = oracle("btrfs")
        .args(["inspect-internal", "dump-super", "-f"])
        .arg(image)
        .output();
    assert!(
        out.status.success(),
        "btrfs inspect-internal dump-super -f {}:\n{}",
        image.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `btrfs inspect-internal dump-tree -t <tree>` on `image`, in the guest.
#[track_caller]
pub fn dump_tree(image: &Path, tree: &str) -> String {
    let out = oracle("btrfs")
        .args(["inspect-internal", "dump-tree", "-t", tree])
        .arg(image)
        .output();
    assert!(
        out.status.success(),
        "btrfs inspect-internal dump-tree -t {tree} {}:\n{}",
        image.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn tree_item_header(line: &str, key: &str) -> bool {
    line.trim_start().starts_with("item ") && line.contains(key) && line.contains(" itemoff ")
}

/// The first leaf item whose key holds `key`: its header line, then its body
/// up to the next line naming a key, so a body that names one (a directory
/// entry's) is cut there. A node's child pointer carries the same key as the
/// first item of its leaf and is passed over (#782).
pub fn tree_item<'a>(dump: &'a str, key: &str) -> Option<Vec<&'a str>> {
    let mut lines = dump.lines().skip_while(|line| !tree_item_header(line, key));
    let header = lines.next()?;
    let body = lines.take_while(|line| !line.contains("key ("));
    Some(std::iter::once(header).chain(body).collect())
}

/// How many leaf items have a key holding `key`. Node pointers are not items.
pub fn tree_item_count(dump: &str, key: &str) -> usize {
    dump.lines()
        .filter(|line| tree_item_header(line, key))
        .count()
}

/// SHA-256 of some bytes, as the hex `sha256sum` prints in the guest.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether an image's superblock says the filesystem spans more than one
/// device.
///
/// Most oracles here read a logical address straight out of a flat
/// image, or require every image to mount. Neither holds for one member
/// of a multi-device filesystem: its chunks may live on the other disk,
/// so a flat read returns whatever lies at that offset — bytes that
/// parse and then fail a checksum against a block they were never meant
/// to be — and mounting it is refused on purpose.
///
/// So a pool member is not a fixture for those tests, and this is how
/// they tell. `tests/pool_oracle.rs` is where such an image IS the
/// subject.
///
/// Read from the raw bytes rather than through the parser, because the
/// point is to decide whether to involve the parser at all.
pub fn spans_several_devices(image: &Path) -> bool {
    /// `num_devices`, at 0x88 within the superblock at 64 KiB.
    const NUM_DEVICES: u64 = 0x1_0000 + 0x88;
    let mut field = [0u8; 8];
    Image::open(image).try_read_at(NUM_DEVICES, &mut field) && u64::from_le_bytes(field) > 1
}

/// A fixture image, read a window at a time rather than loaded whole.
///
/// EIGHT BYTES SHOULD NOT COST TWO GIGABYTES, and until this existed
/// they did. Every suite that walks the fixtures used `std::fs::read`,
/// which allocates the file's whole apparent length: the two populated
/// images are 2 GiB of mostly hole, so reading one to look at a 16 KiB
/// tree block was 2 GiB of zeroes in memory and 2 GiB off the disk.
///
/// That is a waste on a workstation and a wall in the harness guest,
/// which has 4 GiB. `cargo test` runs one test binary at a time but four
/// threads inside it, and four threads each holding one of those images
/// is eight gigabytes: the guest's kernel killed `btree_oracle` outright
/// (SIGKILL), so `chore test:vm` — the whole macOS path — could not get
/// past the fourth suite. Reading windows instead keeps a walker's
/// resident set at one tree block, and takes the 9p traffic with it.
///
/// Not memory-mapped, deliberately: in the guest these images are on a
/// 9p mount, where mmap is at the mercy of the cache mode, and the
/// failure would be a SIGBUS in the one place this most needs to work.
pub struct Image {
    file: fs::File,
    path: PathBuf,
}

impl Image {
    /// Open `path` for reading, or panic naming it and the task that
    /// builds it. Fixtures never skip.
    #[track_caller]
    pub fn open(path: &Path) -> Self {
        let file = fs::File::open(path).unwrap_or_else(|error| {
            panic!(
                "cannot open {}: {error}. The fixtures are built by `chore fixtures`.",
                path.display()
            )
        });
        Image {
            file,
            path: path.to_path_buf(),
        }
    }

    /// The image's length in bytes.
    #[track_caller]
    pub fn len(&self) -> u64 {
        self.file
            .metadata()
            .unwrap_or_else(|error| panic!("cannot stat {}: {error}", self.path.display()))
            .len()
    }

    /// Whether the image is empty — there to satisfy clippy beside
    /// [`Image::len`], and true only of a fixture that failed to build.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fill `buf` from `offset`, or `false` if the image does not reach
    /// that far. A short read inside the image is an error, not a
    /// `false`: the file is a fixed-size disk image, so a window that
    /// starts inside it and ends inside it is always there.
    #[track_caller]
    pub fn try_read_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        use std::os::unix::fs::FileExt;
        if offset.saturating_add(buf.len() as u64) > self.len() {
            return false;
        }
        self.file
            .read_exact_at(buf, offset)
            .unwrap_or_else(|error| {
                panic!(
                    "cannot read {} bytes at {offset} of {}: {error}",
                    buf.len(),
                    self.path.display()
                )
            });
        true
    }

    /// `len` bytes at `offset`, or a panic naming the image: a window
    /// past the end of a fixture is a broken test, not a case to handle.
    #[track_caller]
    pub fn read_at(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        assert!(
            self.try_read_at(offset, &mut buf),
            "{}: {len} bytes at {offset} run past the {}-byte image",
            self.path.display(),
            self.len()
        );
        buf
    }
}

/// Little-endian `u32` at `at`.
///
/// Hand-rolled here, and **deliberately not** `src/`'s reader. These
/// oracles decode leaves by hand on purpose: one that read a leaf
/// through `btree::TreeBlock` would be checking the writer against the
/// reader rather than against the disk, and the two agreeing is exactly
/// what an oracle must not assume.
///
/// # Panics
///
/// If `at + 4` is past the end. A fixture that is too short is a broken
/// test, not a case to handle.
pub fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes in range"))
}

/// Little-endian `u16` at `at`. See [`le32`] on why this is not the
/// crate's own reader.
pub fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().expect("2 bytes in range"))
}

/// Little-endian `u64` at `at`. See [`le32`] on why this is not the
/// crate's own reader.
///
/// # Panics
///
/// If `at + 8` is past the end.
pub fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes in range"))
}
