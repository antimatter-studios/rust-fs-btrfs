//! The C ABI, exercised the way a C caller would use it.
//!
//! This layer is what a consuming application links against, so a defect
//! here reaches users even though every Rust-level test passes. The
//! sibling EROFS driver shipped with this surface at 0% coverage; these
//! tests exist so this crate does not repeat that.
//!
//! Two classes of behaviour get more attention below than the happy
//! paths, because a safe Rust API never has to think about them:
//!
//! - **NULL tolerance.** Every pointer parameter must be checked, not
//!   dereferenced. A caller passing NULL should get a failure, not a
//!   crash inside its own process.
//! - **Error reporting.** A C caller has only the return value, the
//!   thread-local message and the errno. A wrong errno misdirects:
//!   reporting EIO for a missing file sends a user hunting for hardware
//!   faults.
//!
//! The fixture is `btrfs-rich`, written through a compressing mount and
//! holding a compressible file, an incompressible one, an inline file, a
//! sparse file, a symlink and a nested directory. Fixtures are
//! gitignored, so these skip cleanly on a fresh clone.

use fs_btrfs::capi::*;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};

/// Errno values the header documents. Spelled out rather than imported,
/// so the test asserts the contract rather than mirroring the source.
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const ERANGE: i32 = 34;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

fn fixture() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".vm-share")
        .join("btrfs-rich.img");
    p.exists().then_some(p)
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn mount() -> Option<*mut fs_btrfs_fs> {
    let path = fixture()?;
    let c = cstr(path.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount(c.as_ptr()) };
    assert!(
        !fs.is_null(),
        "mounting the fixture failed: {}",
        last_error()
    );
    Some(fs)
}

fn last_errno_erange() -> i32 {
    fs_btrfs_last_errno()
}

fn zeroed_attr() -> fs_btrfs_attr_t {
    unsafe { std::mem::zeroed() }
}

// ---------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------

#[test]
fn mounts_and_reports_volume_info() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut info: fs_btrfs_volume_info_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { fs_btrfs_get_volume_info(fs, &mut info) }, 0);

    assert!(
        info.sector_size.is_power_of_two() && info.sector_size >= 512,
        "sector size {} is not sane",
        info.sector_size
    );
    assert!(
        info.node_size.is_power_of_two() && info.node_size >= info.sector_size,
        "node size {} is not sane",
        info.node_size
    );
    assert!(info.num_devices >= 1);
    assert!(
        info.bytes_used <= info.total_bytes,
        "more bytes used than the device holds"
    );
    assert_ne!(info.fsid, [0u8; 16], "a real filesystem has an fsid");
    // The fixture is made with default (crc32c) checksums.
    assert_eq!(info.csum_type, 0, "expected crc32c on the rich fixture");

    // With METADATA_UUID off, the effective metadata uuid is the fsid.
    assert_eq!(info.metadata_uuid, info.fsid);

    unsafe { fs_btrfs_umount(fs) };
}

#[test]
fn stats_a_file_a_directory_and_a_symlink() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };

    let mut f = zeroed_attr();
    assert_eq!(
        unsafe { fs_btrfs_stat(fs, cstr("/inline.txt").as_ptr(), &mut f) },
        0
    );
    assert_eq!(f.file_type, 1, "inline.txt should be a regular file");
    assert_eq!(f.size, 13, "\"small inline\\n\" is 13 bytes");
    assert!(f.link_count >= 1);

    let mut d = zeroed_attr();
    assert_eq!(
        unsafe { fs_btrfs_stat(fs, cstr("/sub").as_ptr(), &mut d) },
        0
    );
    assert_eq!(d.file_type, 2, "sub should be a directory");

    // stat must describe the link itself, not its target — otherwise a
    // caller cannot tell one from the other.
    let mut l = zeroed_attr();
    assert_eq!(
        unsafe { fs_btrfs_stat(fs, cstr("/link-short").as_ptr(), &mut l) },
        0
    );
    assert_eq!(l.file_type, 7, "link-short should report as a symlink");

    // stat by inode must agree with stat by path.
    let mut by_ino = zeroed_attr();
    assert_eq!(unsafe { fs_btrfs_stat_ino(fs, f.inode, &mut by_ino) }, 0);
    assert_eq!(by_ino.inode, f.inode);
    assert_eq!(by_ino.size, f.size);
    assert_eq!(by_ino.mode, f.mode);

    unsafe { fs_btrfs_umount(fs) };
}

#[test]
fn iterates_a_directory_to_completion() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let iter = unsafe { fs_btrfs_dir_open(fs, cstr("/").as_ptr()) };
    assert!(!iter.is_null(), "opening the root failed: {}", last_error());

    let mut names = Vec::new();
    loop {
        let ptr = unsafe { fs_btrfs_dir_next(iter) };
        if ptr.is_null() {
            assert_eq!(
                fs_btrfs_last_errno(),
                0,
                "a clean end of directory must not set an errno: {}",
                last_error()
            );
            break;
        }
        let e = unsafe { &*ptr };
        let name = unsafe { CStr::from_ptr(e.name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            name.len(),
            usize::from(e.name_len),
            "name_len disagrees with the NUL-terminated name"
        );
        assert_ne!(e.inode, 0, "entry `{name}` has inode 0");
        assert_eq!(e.is_subvolume, 0, "`{name}` is unexpectedly a subvolume");
        names.push(name);
    }

    // Past the end it must keep returning 0 rather than wrapping.
    assert!(unsafe { fs_btrfs_dir_next(iter) }.is_null());
    unsafe { fs_btrfs_dir_close(iter) };

    for want in ["inline.txt", "plain.bin", "sparse.bin", "link-short", "sub"] {
        assert!(
            names.contains(&want.to_string()),
            "missing {want}: {names:?}"
        );
    }
    // `.` and `..` must never appear, matching the sibling driver.
    assert!(!names.iter().any(|n| n == "." || n == ".."));

    unsafe { fs_btrfs_umount(fs) };
}

#[test]
fn reads_file_contents() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = [0u8; 64];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            cstr("/inline.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert!(n > 0, "read failed: {}", last_error());
    assert_eq!(&buf[..n as usize], b"small inline\n");

    // From an offset, the tail rather than the head again.
    let mut tail = [0u8; 64];
    let m = unsafe {
        fs_btrfs_read_file(
            fs,
            cstr("/inline.txt").as_ptr(),
            tail.as_mut_ptr().cast::<c_void>(),
            6,
            tail.len() as u64,
        )
    };
    assert_eq!(&tail[..m as usize], b"inline\n");

    // At end of file: zero bytes, not an error.
    let at_eof = unsafe {
        fs_btrfs_read_file(
            fs,
            cstr("/inline.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            n as u64,
            buf.len() as u64,
        )
    };
    assert_eq!(at_eof, 0);

    unsafe { fs_btrfs_umount(fs) };
}

/// A hole must read as zeros rather than as whatever previously occupied
/// those blocks.
#[test]
fn sparse_regions_read_as_zeros() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = vec![0xAAu8; 65536];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            cstr("/sparse.bin").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert!(n > 0, "read failed: {}", last_error());
    assert!(
        buf[..n as usize].iter().all(|&b| b == 0),
        "a hole did not read back as zeros"
    );
    unsafe { fs_btrfs_umount(fs) };
}

#[test]
fn reads_a_symlink_target() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = [0 as c_char; 512];
    let n = unsafe {
        fs_btrfs_readlink(
            fs,
            cstr("/link-short").as_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    assert!(n > 0, "readlink failed: {}", last_error());
    let target = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    assert_eq!(target, "inline.txt");
    assert_eq!(usize::try_from(n).unwrap(), target.len());
    unsafe { fs_btrfs_umount(fs) };
}

/// A buffer too small for the target is REFUSED rather than truncated.
///
/// A truncated symlink target is a path to somewhere else, and a caller
/// following it has no way to tell. ERANGE tells it to retry with a
/// larger buffer, which is the standard idiom — and matches the sibling
/// EROFS driver, so the family agrees.
#[test]
fn readlink_refuses_a_buffer_too_small_for_the_target() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = [0x7F as c_char; 5];
    let n = unsafe {
        fs_btrfs_readlink(
            fs,
            cstr("/link-short").as_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    assert_eq!(n, -1, "a target that does not fit must be refused");
    assert_eq!(
        last_errno_erange(),
        ERANGE,
        "a buffer too small is ERANGE, got {}",
        last_errno_erange()
    );
    assert!(
        buf.iter().all(|&c| c as u8 == 0x7F),
        "a refused readlink must not have written into the buffer"
    );
    unsafe { fs_btrfs_umount(fs) };
}

// ---------------------------------------------------------------------
// Error reporting
// ---------------------------------------------------------------------

/// The single most important refusal in this driver, surfaced through
/// the ABI. Returning the compressed bytes would look to a caller
/// exactly like a successful read of a corrupt file.
#[test]
fn a_compressed_file_fails_with_enotsup_not_garbage() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            cstr("/compressed.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(n, -1, "a compressed extent must not be read as raw bytes");
    assert_eq!(
        fs_btrfs_last_errno(),
        ENOTSUP,
        "a feature this driver declines is ENOTSUP, got {}",
        fs_btrfs_last_errno()
    );
    assert!(
        last_error().contains("compression"),
        "the message should name compression: {}",
        last_error()
    );
    unsafe { fs_btrfs_umount(fs) };
}

/// And an ordinary file on the same filesystem must still read, or the
/// test above would pass on a driver that refused everything.
#[test]
fn an_uncompressed_file_on_the_same_volume_still_reads() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            cstr("/plain.bin").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(n, 4096, "plain.bin should read: {}", last_error());
    assert!(buf.iter().any(|&b| b != 0), "random data read as zeros");
    unsafe { fs_btrfs_umount(fs) };
}

#[test]
fn a_missing_path_reports_enoent_not_eio() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut a = zeroed_attr();
    assert_eq!(
        unsafe { fs_btrfs_stat(fs, cstr("/no-such-file").as_ptr(), &mut a) },
        -1
    );
    assert_eq!(fs_btrfs_last_errno(), ENOENT);
    assert!(!last_error().is_empty());
    unsafe { fs_btrfs_umount(fs) };
}

#[test]
fn listing_a_file_reports_enotdir() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let iter = unsafe { fs_btrfs_dir_open(fs, cstr("/inline.txt").as_ptr()) };
    assert!(
        iter.is_null(),
        "a regular file must not open as a directory"
    );
    assert_eq!(fs_btrfs_last_errno(), ENOTDIR);
    unsafe { fs_btrfs_umount(fs) };
}

#[test]
fn reading_a_directory_as_a_file_reports_eisdir() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = [0u8; 16];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            cstr("/sub").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(n, -1);
    assert_eq!(fs_btrfs_last_errno(), EISDIR);
    unsafe { fs_btrfs_umount(fs) };
}

#[test]
fn mounting_a_non_btrfs_file_fails_with_a_message() {
    let tmp = std::env::temp_dir().join(format!("capi-notbtrfs-{}.img", std::process::id()));
    std::fs::write(&tmp, vec![0x5Au8; 256 * 1024]).unwrap();
    let c = cstr(tmp.to_str().unwrap());
    let fs = unsafe { fs_btrfs_mount(c.as_ptr()) };
    assert!(fs.is_null(), "a file of 0x5A must not mount as Btrfs");
    assert_eq!(fs_btrfs_last_errno(), EIO);
    assert!(
        last_error().to_lowercase().contains("btrfs"),
        "message should name the format: {}",
        last_error()
    );
    std::fs::remove_file(&tmp).ok();
}

/// A path that does not exist is the caller's mistake, not damaged
/// media, so it must not be reported as an I/O error.
#[test]
fn mounting_a_missing_path_reports_enoent() {
    let fs = unsafe { fs_btrfs_mount(cstr("/nonexistent/device.img").as_ptr()) };
    assert!(fs.is_null());
    assert_eq!(
        fs_btrfs_last_errno(),
        ENOENT,
        "a path that does not exist must be ENOENT, not EIO"
    );
    assert!(!last_error().is_empty());
}

// ---------------------------------------------------------------------
// NULL tolerance
// ---------------------------------------------------------------------

#[test]
fn null_pointers_fail_instead_of_crashing() {
    let mut attr = zeroed_attr();
    let mut info: fs_btrfs_volume_info_t = unsafe { std::mem::zeroed() };
    let mut buf = [0u8; 8];
    let mut cbuf = [0 as c_char; 8];
    let p = cstr("/x");

    unsafe {
        assert!(fs_btrfs_mount(std::ptr::null()).is_null());
        assert!(fs_btrfs_mount_with_callbacks(std::ptr::null()).is_null());

        assert_eq!(
            fs_btrfs_get_volume_info(std::ptr::null_mut(), &mut info),
            -1
        );
        assert_eq!(
            fs_btrfs_stat(std::ptr::null_mut(), p.as_ptr(), &mut attr),
            -1
        );
        assert_eq!(fs_btrfs_stat_ino(std::ptr::null_mut(), 1, &mut attr), -1);
        assert!(fs_btrfs_dir_open(std::ptr::null_mut(), p.as_ptr()).is_null());
        assert!(fs_btrfs_dir_next(std::ptr::null_mut()).is_null());
        assert_eq!(
            fs_btrfs_read_file(
                std::ptr::null_mut(),
                p.as_ptr(),
                buf.as_mut_ptr().cast::<c_void>(),
                0,
                buf.len() as u64
            ),
            -1
        );
        assert_eq!(
            fs_btrfs_readlink(
                std::ptr::null_mut(),
                p.as_ptr(),
                cbuf.as_mut_ptr(),
                cbuf.len()
            ),
            -1
        );

        // Releasing NULL must be a safe no-op, as the header promises.
        fs_btrfs_umount(std::ptr::null_mut());
        fs_btrfs_dir_close(std::ptr::null_mut());
    }
}

#[test]
fn null_output_pointers_fail_instead_of_crashing() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    unsafe {
        assert_eq!(fs_btrfs_get_volume_info(fs, std::ptr::null_mut()), -1);
        assert_eq!(
            fs_btrfs_stat(fs, cstr("/").as_ptr(), std::ptr::null_mut()),
            -1
        );
        assert_eq!(fs_btrfs_stat_ino(fs, 256, std::ptr::null_mut()), -1);
        assert_eq!(
            fs_btrfs_read_file(fs, cstr("/inline.txt").as_ptr(), std::ptr::null_mut(), 0, 8),
            -1
        );
        assert_eq!(
            fs_btrfs_readlink(fs, cstr("/link-short").as_ptr(), std::ptr::null_mut(), 8),
            -1
        );
        // A zero-length buffer leaves no room even for the terminator.
        let mut one = [0 as c_char; 1];
        assert!(fs_btrfs_readlink(fs, cstr("/link-short").as_ptr(), one.as_mut_ptr(), 0) < 0);

        // A NULL path is a failure, not a dereference.
        let mut attr = zeroed_attr();
        assert_eq!(fs_btrfs_stat(fs, std::ptr::null(), &mut attr), -1);
        assert!(fs_btrfs_dir_open(fs, std::ptr::null()).is_null());

        fs_btrfs_umount(fs);
    }
}

/// A non-UTF-8 path is rejected rather than misinterpreted.
#[test]
fn a_non_utf8_path_is_rejected() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let bad = [b'/' as c_char, 0xFFu8 as c_char, 0];
    let mut attr = zeroed_attr();
    assert_eq!(unsafe { fs_btrfs_stat(fs, bad.as_ptr(), &mut attr) }, -1);
    assert!(!last_error().is_empty());
    unsafe { fs_btrfs_umount(fs) };
}

// ---------------------------------------------------------------------
// The callback mount path
// ---------------------------------------------------------------------

struct FileContext {
    bytes: Vec<u8>,
    /// Set to make every read fail, proving failures surface.
    fail: bool,
}

unsafe extern "C" fn ctx_read(
    context: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> i32 {
    let ctx = unsafe { &*(context as *const FileContext) };
    if ctx.fail {
        return -1;
    }
    let start = offset as usize;
    let end = start.saturating_add(length as usize);
    if end > ctx.bytes.len() {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            ctx.bytes[start..end].as_ptr(),
            buf.cast::<u8>(),
            end - start,
        )
    };
    0
}

#[test]
fn mounts_over_a_caller_supplied_reader() {
    let Some(img) = fixture() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let ctx = Box::new(FileContext {
        bytes: std::fs::read(&img).unwrap(),
        fail: false,
    });
    let size = ctx.bytes.len() as u64;
    let cfg = fs_btrfs_blockdev_cfg_t {
        read: Some(ctx_read),
        context: Box::into_raw(ctx) as *mut c_void,
        size_bytes: size,
        block_size: 4096,
    };
    let fs = unsafe { fs_btrfs_mount_with_callbacks(&cfg) };
    assert!(!fs.is_null(), "callback mount failed: {}", last_error());

    // It must actually work, not merely mount.
    let mut buf = [0u8; 64];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            cstr("/inline.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(&buf[..n as usize], b"small inline\n");

    unsafe { fs_btrfs_umount(fs) };
    drop(unsafe { Box::from_raw(cfg.context as *mut FileContext) });
}

/// A callback that fails must surface as an error, never as silently
/// zeroed data — a caller cannot tell those apart.
#[test]
fn a_failing_callback_surfaces_as_an_error() {
    let ctx = Box::new(FileContext {
        bytes: vec![0u8; 256 * 1024],
        fail: true,
    });
    let cfg = fs_btrfs_blockdev_cfg_t {
        read: Some(ctx_read),
        context: Box::into_raw(ctx) as *mut c_void,
        size_bytes: 256 * 1024,
        block_size: 4096,
    };
    let fs = unsafe { fs_btrfs_mount_with_callbacks(&cfg) };
    assert!(fs.is_null(), "a failing reader must not produce a handle");
    assert!(!last_error().is_empty());
    drop(unsafe { Box::from_raw(cfg.context as *mut FileContext) });
}

#[test]
fn a_null_callback_is_rejected() {
    let cfg = fs_btrfs_blockdev_cfg_t {
        read: None,
        context: std::ptr::null_mut(),
        size_bytes: 4096,
        block_size: 4096,
    };
    assert!(unsafe { fs_btrfs_mount_with_callbacks(&cfg) }.is_null());
    assert!(!last_error().is_empty());
}

/// `fs_btrfs_last_error` must never return NULL, including before any
/// failure — a C caller will print it unconditionally.
#[test]
fn last_error_is_never_null() {
    assert!(!fs_btrfs_last_error().is_null());
    assert!(!last_error().is_empty());
}
