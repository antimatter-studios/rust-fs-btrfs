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
    guest_kernel_probe, guest_kernel_read, guest_kernel_read_ok, guest_kernel_report,
    guest_kernel_write, guest_kernel_write_ok, KernelProbe,
};
pub use oracle::{guest_base64, guest_quote, oracle, Oracle};

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
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

/// SHA-256 of some bytes, as the hex `sha256sum` prints in the guest.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
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
        self.file.read_exact_at(buf, offset).unwrap_or_else(|error| {
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
