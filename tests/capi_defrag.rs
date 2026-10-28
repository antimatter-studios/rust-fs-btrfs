//! `fs_btrfs_defragment` (#305): the library's defragment reached through
//! the C ABI, on this crate's own mkfs volume.
//!
//! Each case defragments the same file twice from one starting image:
//! once through the C ABI and once through the library on a twin copy.
//! The two images must come out byte-identical and the reports equal, so
//! the C ABI adds nothing to the change and loses nothing of it. What the
//! kernel and `btrfs check` make of the library's defragment is
//! tests/defrag_kernel.rs, and of one made through `fs.btrfs`,
//! tests/cli_defrag_kernel.rs.
//!
//! No fixture, no VM: the unit tier.

use fs_btrfs::capi::*;
use fs_btrfs::{mkfs, Filesystem};
use fs_btrfs_test_support::temp_path;
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ENOENT: i32 = 2;
const EISDIR: i32 = 21;
const EROFS: i32 = 30;

/// `ENOTSUP` is 45 on Darwin and 95 on Linux.
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

/// The appends each fragmented file is made of.
const APPENDS: usize = 6;
const APPEND: usize = 8192;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

/// Bytes that differ from append to append and from file to file.
fn pattern(file: u8, append: usize) -> Vec<u8> {
    (0..APPEND)
        .map(|i| (i as u8).wrapping_mul(31) ^ file ^ (append as u8).wrapping_mul(7))
        .collect()
}

/// A scratch volume holding two files, `frag` and `other`, each written
/// as [`APPENDS`] appends committed one at a time and taking turns, so
/// each append is an extent of its own; a file written in one go,
/// `whole`; and a directory, `dir`. Removed when it drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(temp_path!("capi-defrag-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(128 << 20))
            .unwrap();
        let dev = FileDevice::open_rw(&image).unwrap();
        mkfs::format(&dev, &mkfs::Options::default()).unwrap();
        drop(dev);
        let mut fs = mount_rw(&image);
        let root = fs.lookup_path("/").unwrap().ino;
        let frag = fs.create(root, b"frag", 0o644, 0, 0).unwrap();
        let other = fs.create(root, b"other", 0o644, 0, 0).unwrap();
        for n in 0..APPENDS {
            let at = (n * APPEND) as u64;
            fs.write(frag, at, &pattern(1, n)).unwrap();
            fs.write(other, at, &pattern(2, n)).unwrap();
        }
        let whole = fs.create(root, b"whole", 0o644, 0, 0).unwrap();
        fs.write(whole, 0, &vec![0x5a; 3 * APPEND]).unwrap();
        fs.mkdir(root, b"dir", 0o755, 0, 0).unwrap();
        Self(dir)
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }

    /// A byte-for-byte copy of the image beside it.
    fn twin(&self) -> PathBuf {
        let twin = self.0.join("twin.img");
        std::fs::copy(self.image(), &twin).unwrap();
        twin
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open read-write"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write")
}

fn c_mount_rw(image: &Path) -> *mut fs_btrfs_fs {
    let c = cstr(image.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount_rw(c.as_ptr()) };
    assert!(!fs.is_null(), "mount rw: {}", last_error());
    fs
}

fn c_mount(image: &Path) -> *mut fs_btrfs_fs {
    let c = cstr(image.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount(c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_error());
    fs
}

/// `fs_btrfs_defragment` on `path`: its return code, and its report.
fn defragment(fs: *mut fs_btrfs_fs, path: &str, target: u64) -> (i32, fs_btrfs_defrag_t) {
    let c = cstr(path);
    let mut out = fs_btrfs_defrag_t {
        items_before: u64::MAX,
        items_after: u64::MAX,
        runs: u64::MAX,
        bytes: u64::MAX,
    };
    let rc = unsafe { fs_btrfs_defragment(fs, c.as_ptr(), target, &mut out) };
    (rc, out)
}

fn report(out: &fs_btrfs_defrag_t) -> (u64, u64, u64, u64) {
    (out.items_before, out.items_after, out.runs, out.bytes)
}

fn read_all(fs: *mut fs_btrfs_fs, path: &str) -> Vec<u8> {
    let c = cstr(path);
    let mut buf = vec![0u8; APPENDS * APPEND + 4096];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            c.as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert!(n >= 0, "reading {path}: {}", last_error());
    buf.truncate(n as usize);
    buf
}

/// The fragmented file is rewritten as one extent: the report counts its
/// appends before and one item after, the file reads the same bytes, the
/// image is the one the library's own defragment makes of a twin, and a
/// second defragment finds nothing to do and writes nothing.
#[test]
fn defragment_rewrites_the_file_as_the_library_does() {
    let scratch = Scratch::new("default");
    let image = scratch.image();
    let twin = scratch.twin();
    let want: Vec<u8> = (0..APPENDS).flat_map(|n| pattern(1, n)).collect();

    let fs = c_mount_rw(&image);
    assert_eq!(read_all(fs, "/frag"), want, "the fill reads back");
    let (rc, out) = defragment(fs, "/frag", 0);
    assert_eq!(rc, 0, "defragment /frag: {}", last_error());
    let expect = (APPENDS as u64, 1, 1, (APPENDS * APPEND) as u64);
    assert_eq!(report(&out), expect, "a target of 0 is the default one");
    assert_eq!(read_all(fs, "/frag"), want, "/frag after the defragment");
    unsafe { fs_btrfs_umount(fs) };

    let mut lib = mount_rw(&twin);
    let ino = lib.lookup_path("/frag").unwrap().ino;
    let done = lib.defragment(ino).unwrap();
    drop(lib);
    assert_eq!(
        (
            done.items_before as u64,
            done.items_after as u64,
            done.runs as u64,
            done.bytes
        ),
        expect,
        "the library's own defragment"
    );
    assert!(
        std::fs::read(&image).unwrap() == std::fs::read(&twin).unwrap(),
        "the C ABI's defragment and the library's left different images"
    );

    let fs = c_mount_rw(&image);
    let before = std::fs::read(&image).unwrap();
    let (rc, out) = defragment(fs, "/frag", 0);
    assert_eq!(rc, 0, "defragment /frag again: {}", last_error());
    assert_eq!(report(&out), (1, 1, 0, 0), "nothing left to do");
    let (rc, out) = defragment(fs, "/whole", 0);
    assert_eq!(rc, 0, "defragment /whole: {}", last_error());
    assert_eq!(report(&out), (1, 1, 0, 0), "a file of one extent");
    unsafe { fs_btrfs_umount(fs) };
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a defragment with nothing to do wrote to the image"
    );
}

/// A target splits the file into runs no longer than it, as the
/// library's `defragment_to` does, and a NULL report is allowed.
#[test]
fn a_target_bounds_each_run_as_defragment_to_does() {
    let scratch = Scratch::new("target");
    let image = scratch.image();
    let twin = scratch.twin();
    let target = 2 * APPEND as u64;

    let fs = c_mount_rw(&image);
    let (rc, out) = defragment(fs, "/other", target);
    assert_eq!(rc, 0, "defragment /other: {}", last_error());
    let runs = (APPENDS / 2) as u64;
    assert_eq!(
        report(&out),
        (APPENDS as u64, runs, runs, (APPENDS * APPEND) as u64),
        "runs of two appends each"
    );
    let path = cstr("/frag");
    let rc = unsafe { fs_btrfs_defragment(fs, path.as_ptr(), target, std::ptr::null_mut()) };
    assert_eq!(rc, 0, "defragment /frag with no report: {}", last_error());
    unsafe { fs_btrfs_umount(fs) };

    let mut lib = mount_rw(&twin);
    for name in ["/other", "/frag"] {
        let ino = lib.lookup_path(name).unwrap().ino;
        lib.defragment_to(ino, target).unwrap();
    }
    drop(lib);
    assert!(
        std::fs::read(&image).unwrap() == std::fs::read(&twin).unwrap(),
        "the C ABI's defragment to a target and the library's left different images"
    );
}

/// Each refusal names its errno and leaves the image byte-identical.
#[test]
fn refusals_set_their_errno_and_write_nothing() {
    let scratch = Scratch::new("refusals");
    let image = scratch.image();
    let before = std::fs::read(&image).unwrap();

    let fs = c_mount_rw(&image);
    for (path, target, errno, what) in [
        ("/missing", 0, ENOENT, "a path that does not exist"),
        ("/dir", 0, EISDIR, "a directory"),
        ("/frag", 1, ENOTSUP, "a target shorter than a sector"),
    ] {
        let (rc, _) = defragment(fs, path, target);
        assert_eq!(rc, -1, "{what} was defragmented");
        assert_eq!(fs_btrfs_last_errno(), errno, "{what}: {}", last_error());
    }
    let rc = unsafe { fs_btrfs_defragment(fs, std::ptr::null(), 0, std::ptr::null_mut()) };
    assert_eq!(rc, -1, "a NULL path was defragmented");
    unsafe { fs_btrfs_umount(fs) };

    let ro = c_mount(&image);
    let (rc, _) = defragment(ro, "/frag", 0);
    assert_eq!(rc, -1, "a read-only handle defragmented");
    assert_eq!(fs_btrfs_last_errno(), EROFS, "{}", last_error());
    unsafe { fs_btrfs_umount(ro) };

    let rc = unsafe {
        fs_btrfs_defragment(
            std::ptr::null_mut(),
            cstr("/frag").as_ptr(),
            0,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, -1, "a NULL handle defragmented");

    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused defragment wrote to the image"
    );

    // The library is still the judge of what the file holds.
    let dev = Arc::new(FileDevice::open(&image).unwrap());
    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).unwrap();
    let ino = fs.lookup_path("/frag").unwrap().ino;
    let want: Vec<u8> = (0..APPENDS).flat_map(|n| pattern(1, n)).collect();
    assert_eq!(fs.read_file(ino).unwrap(), want);
}
