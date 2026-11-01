//! `import`: stream a local file, stdin or a local tree into the image
//! through rust-fs-core's shared import, publishing a file only when it is
//! complete (#431). This file is the shared sink over this library's
//! write API:
//!
//! - **stage**: an empty file with a private name,
//!   `.fs-btrfs-import-PID-N.partial`, is created beside the destination,
//!   in the destination's directory, as one committed transaction;
//! - **write**: bytes go in at increasing offsets, a bounded chunk at a
//!   time, through the library's copy-on-write `Filesystem::write`, which
//!   allocates and grows the file and commits each call as a transaction
//!   of its own;
//! - **persist**: what is gathered is written, so every byte is in a
//!   committed transaction: on the device, with the superblocks pointing
//!   at it (`Filesystem::commit`'s data, trees, flush, superblocks,
//!   flush);
//! - **read back**: for `--verify`, through a fresh read-only open of the
//!   image, never through the mount the writes went to;
//! - **publish**: the staged name is renamed to the final name. The
//!   library's rename is one committed transaction, and it replaces a
//!   name already there in that same transaction, the replaced file's
//!   data released with it, which is why `--replace` is offered. The old
//!   file is never deleted first;
//! - **abort**: the staged name is unlinked, and its data released with
//!   it. The final name is not touched.
//!
//! The guarantee that covers publication is Btrfs's own: a transaction
//! is reached only through the superblocks, written last, so a crash
//! leaves the volume at the last committed generation and never between
//! two. The destination's name is changed by exactly one transaction, the
//! rename, so it holds the old file (or nothing) or the whole new one.
//!
//! A crash between persist and publish leaves the old file (or no file)
//! under the final name and the whole staged file beside it, under its
//! staged name: the volume is consistent, and the leftover is an
//! ordinary file that `rm` removes. A crash while writing leaves a
//! shorter staged file, just as consistent.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, ArgMatches};
use fs_btrfs::inode::Inode;
use fs_btrfs::{Error, Filesystem};
use fs_core::cli::transfer::{
    import_args_with_tree, import_tree_with_sparse, import_with_sparse, FileSink, ImportOptions,
    ImportRequest, ImportTreeOptions, SparseOptions, TreeSink,
};
use fs_core::cli::{CliError, Outcome};

use super::device;
use super::fs::{btrfs_error, show, state_error};

/// The permission bits of an imported file and of a directory a tree
/// import makes: `create`'s and `mkdir`'s defaults.
const FILE_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o755;

/// The longest name a Btrfs directory entry holds.
const NAME_MAX: usize = 255;

/// How much is gathered before it is written. Each call to the library's
/// write is a committed transaction, and its bytes one extent that must
/// fit in one free run of a data group, so this bounds both the memory
/// held and the run asked for, and keeps the transactions few.
const GATHER: usize = 4 << 20;

/// The arguments `import` takes: the shared tree set, and `--verify`.
pub fn args() -> [Arg; 7] {
    let [source, path, replace, recursive, depth, preserve] = import_args_with_tree();
    [
        source,
        path,
        replace,
        recursive,
        depth,
        preserve,
        Arg::new("verify")
            .long("verify")
            .action(ArgAction::SetTrue)
            .help(
                "Read the persisted staged file back and verify its length and SHA-256 \
                 before publishing (one file, not --recursive)",
            ),
    ]
}

/// The file being staged: where it is, and what it will be called.
struct Staged {
    /// The directory both names are in.
    dir: u64,
    /// The private name the bytes go to.
    stage: Vec<u8>,
    /// The name it is published under.
    name: Vec<u8>,
    /// The staged file's inode.
    ino: u64,
    /// The destination as given, for messages.
    path: Vec<u8>,
}

struct Sink<'a> {
    target: &'a OsString,
    offset: u64,
    image: PathBuf,
    /// The writable mount, open from `stage` to `persist`.
    rw: Option<Filesystem>,
    /// A fresh read-only mount, for reading the staged file back, and the
    /// staged file's length as it read it.
    ro: Option<(Filesystem, u64)>,
    staged: Option<Staged>,
    counter: u32,
    /// Bytes accepted by `write_at` and not yet written, and where they go.
    gathered: Vec<u8>,
    gathered_at: u64,
}

impl<'a> Sink<'a> {
    fn new(target: &'a OsString, offset: u64) -> Self {
        Sink {
            target,
            offset,
            image: PathBuf::from(target),
            rw: None,
            ro: None,
            staged: None,
            counter: 0,
            gathered: Vec::new(),
            gathered_at: 0,
        }
    }

    /// The writable mount: the one open, or a fresh one.
    fn mount_rw(&mut self) -> Result<Filesystem, CliError> {
        if let Some(fs) = self.rw.take() {
            return Ok(fs);
        }
        Filesystem::mount_rw(device::open_rw(self.target, self.offset)?).map_err(|e| {
            btrfs_error(
                format!("{} (read-write)", self.target.to_string_lossy()).as_bytes(),
                e,
            )
        })
    }

    /// Write what has been gathered, in one call to the library. A failed
    /// write drops the mount, which may still read the generation before
    /// it; whatever comes next opens a fresh one.
    fn flush(&mut self) -> Result<(), CliError> {
        if self.gathered.is_empty() {
            return Ok(());
        }
        let Some(staged) = self.staged.as_ref() else {
            return Err(CliError::failed("write before stage"));
        };
        let (ino, path) = (staged.ino, staged.path.clone());
        let mut fs = self.mount_rw()?;
        let written = fs.write(ino, self.gathered_at, &self.gathered);
        self.gathered.clear();
        match written {
            Ok(_) => {
                self.rw = Some(fs);
                Ok(())
            }
            Err(e) => Err(btrfs_error(&path, e)),
        }
    }
}

#[cfg(unix)]
fn os_bytes(s: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(s: &OsStr) -> &[u8] {
    s.to_str().map(str::as_bytes).unwrap_or_default()
}

/// A name a Btrfs directory can hold: not empty, `.` or `..`, no NUL, at
/// most 255 bytes. A `/` cannot reach here: it separates the path.
fn checked_name(name: &[u8]) -> Result<(), CliError> {
    if name.is_empty() || name == b"." || name == b".." {
        return Err(CliError::refused(format!(
            "{}: not a usable name",
            show(name)
        )));
    }
    if name.len() > NAME_MAX {
        return Err(CliError::refused(format!(
            "{}: {} bytes, and Btrfs names hold at most {NAME_MAX}",
            show(name),
            name.len()
        )));
    }
    if name.contains(&0) || name.contains(&b'/') {
        return Err(CliError::refused(format!(
            "{}: Btrfs names hold no NUL and no /",
            show(name)
        )));
    }
    Ok(())
}

/// `path`'s directory, resolved in the top-level subvolume, and its last
/// component, checked. A directory inside another subvolume is refused:
/// the namespace writes work in the tree the handle reads.
fn parent_of<'p>(fs: &Filesystem, path: &'p [u8]) -> Result<(Inode, &'p [u8]), CliError> {
    let end = path
        .iter()
        .rposition(|&b| b != b'/')
        .ok_or_else(|| CliError::usage(format!("{}: names no entry", show(path))))?;
    let trimmed = &path[..=end];
    let (dir, name) = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(at) => (&trimmed[..at], &trimmed[at + 1..]),
        None => (&b""[..], trimmed),
    };
    checked_name(name)?;
    let dir = if dir.is_empty() { &b"/"[..] } else { dir };
    let found = fs
        .resolve_path_bytes(dir)
        .map_err(|e| state_error(dir, e))?;
    if !found.inode.is_dir() {
        return Err(CliError::refused_by_state(format!(
            "{}: not a directory",
            show(dir)
        )));
    }
    if found.tree.is_some() {
        return Err(CliError::refused(format!(
            "{}: is inside a subvolume or snapshot, which import does not write yet",
            show(dir)
        )));
    }
    Ok((found.inode, name))
}

/// The regular file at `name` in `dir`, `None` when there is none; any
/// other kind is refused.
fn existing_file(fs: &Filesystem, dir: u64, name: &[u8], path: &[u8]) -> Result<bool, CliError> {
    match fs.lookup(dir, name) {
        Ok(inode) if inode.is_regular_file() => Ok(true),
        Ok(_) => Err(CliError::refused(format!(
            "{}: exists and is not a regular file; --replace replaces files only",
            show(path)
        ))),
        Err(Error::NotFound) => Ok(false),
        Err(e) => Err(btrfs_error(path, e)),
    }
}

impl FileSink for Sink<'_> {
    fn image_path(&self) -> Option<&Path> {
        Some(&self.image)
    }

    fn supports_atomic_replace(&self) -> bool {
        true
    }

    fn stage(&mut self, path: &Path, replace: bool) -> Result<(), CliError> {
        let what = os_bytes(path.as_os_str()).to_vec();
        self.ro = None;
        let mut fs = self.mount_rw()?;
        let (parent, name) = parent_of(&fs, &what)?;
        if existing_file(&fs, parent.ino, name, &what)? && !replace {
            return Err(CliError::failed(format!(
                "{}: exists; give --replace to replace it",
                show(&what)
            )));
        }
        self.counter += 1;
        let stage = format!(
            ".fs-btrfs-import-{}-{}.partial",
            std::process::id(),
            self.counter
        )
        .into_bytes();
        let ino = fs
            .create(parent.ino, &stage, FILE_MODE, parent.uid, parent.gid)
            .map_err(|e| state_error(&what, e))?;
        self.staged = Some(Staged {
            dir: parent.ino,
            stage,
            name: name.to_vec(),
            ino,
            path: what,
        });
        self.rw = Some(fs);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), CliError> {
        if self.staged.is_none() {
            return Err(CliError::failed("write_at before stage"));
        }
        if !self.gathered.is_empty() && self.gathered_at + self.gathered.len() as u64 != offset {
            self.flush()?;
        }
        if self.gathered.is_empty() {
            self.gathered_at = offset;
        }
        self.gathered.extend_from_slice(bytes);
        if self.gathered.len() >= GATHER {
            self.flush()?;
        }
        Ok(())
    }

    fn persist(&mut self) -> Result<(), CliError> {
        self.flush()?;
        // Every write is committed; the mount has nothing left to hold.
        self.rw = None;
        Ok(())
    }

    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<usize, CliError> {
        let Some(staged) = self.staged.as_ref() else {
            return Err(CliError::failed("read_at before stage"));
        };
        if self.ro.is_none() {
            let fs = device::mount(self.target, self.offset)?;
            let len = fs
                .read_inode(staged.ino)
                .map_err(|e| btrfs_error(&staged.path, e))?
                .size;
            self.ro = Some((fs, len));
        }
        let (fs, len) = self.ro.as_ref().expect("mounted above");
        if offset >= *len {
            return Ok(0);
        }
        fs.read_at(staged.ino, offset, bytes)
            .map_err(|e| btrfs_error(&staged.path, e))
    }

    fn publish(&mut self, replace: bool) -> Result<bool, CliError> {
        self.ro = None;
        let Some(staged) = self.staged.as_ref() else {
            return Err(CliError::failed("publish before stage"));
        };
        let (dir, stage, name, what) = (
            staged.dir,
            staged.stage.clone(),
            staged.name.clone(),
            staged.path.clone(),
        );
        let mut fs = self.mount_rw()?;
        let existed = existing_file(&fs, dir, &name, &what)?;
        if existed && !replace {
            return Err(CliError::failed(format!(
                "{}: appeared since it was staged; give --replace to replace it",
                show(&what)
            )));
        }
        fs.rename(dir, &stage, dir, &name)
            .map_err(|e| state_error(&what, e))?;
        self.staged = None;
        Ok(existed)
    }

    fn abort(&mut self) -> Result<(), CliError> {
        self.ro = None;
        self.gathered = Vec::new();
        let Some(staged) = self.staged.take() else {
            self.rw = None;
            return Ok(());
        };
        let removed =
            self.mount_rw()
                .and_then(|mut fs| match fs.unlink(staged.dir, &staged.stage) {
                    Ok(()) | Err(Error::NotFound) => Ok(()),
                    Err(e) => Err(btrfs_error(&staged.path, e)),
                });
        self.rw = None;
        removed.map_err(|e| {
            CliError::failed(format!(
                "{}: the staged file {} could not be removed: {}",
                show(&staged.path),
                show(&staged.stage),
                e.message
            ))
        })
    }
}

impl TreeSink for Sink<'_> {
    fn validate_name(&mut self, _parent: &Path, name: &OsStr) -> Result<OsString, CliError> {
        checked_name(os_bytes(name))?;
        Ok(name.to_os_string())
    }

    fn create_dir(&mut self, path: &Path) -> Result<bool, CliError> {
        let what = os_bytes(path.as_os_str()).to_vec();
        let mut fs = self.mount_rw()?;
        let (parent, name) = parent_of(&fs, &what)?;
        match fs.lookup(parent.ino, name) {
            Ok(existing) if existing.is_dir() => {
                self.rw = Some(fs);
                return Ok(true);
            }
            Ok(_) => {
                return Err(CliError::refused_by_state(format!(
                    "{}: exists and is not a directory",
                    show(&what)
                )))
            }
            Err(Error::NotFound) => {}
            Err(e) => return Err(btrfs_error(&what, e)),
        }
        fs.mkdir(parent.ino, name, DIR_MODE, parent.uid, parent.gid)
            .map_err(|e| state_error(&what, e))?;
        self.rw = Some(fs);
        Ok(false)
    }

    fn format_name(&self) -> &str {
        "Btrfs"
    }
}

/// `import [--recursive] [--replace] [--verify] SOURCE PATH`.
pub fn import(target: &OsString, offset: u64, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let request = ImportRequest::from_matches(sub)?;
    let tree = ImportTreeOptions::from_matches(sub)?;
    let options = ImportOptions::from_matches(sub)?;
    // Holes are not kept: this sink writes every byte, so --preserve=sparse
    // is refused by rust-fs-core rather than quietly filled.
    let sparse = SparseOptions::from_matches(sub)?;
    let mut sink = Sink::new(target, offset);
    if tree.recursive {
        if options.verify {
            return Err(CliError::usage(
                "--verify reads one staged file back; it does not apply to --recursive",
            ));
        }
        return import_tree_with_sparse(&request, Some(&mut sink), tree, sparse);
    }
    import_with_sparse(&request, options, sparse, Some(&mut sink))
}
