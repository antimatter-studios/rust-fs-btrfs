//! Writes through two open handles of one volume, the top level and a
//! subvolume, judged by btrfs-progs and the kernel (#599).
//!
//! Each handle keeps its own copy of the superblock. A commit through one
//! leaves the other holding the generation and root tree it read before,
//! and a commit from there would write the superblock over the first one:
//! the first commit's tree is lost and its generation is used twice. So a
//! handle whose view is older than the volume must refuse to commit, and
//! a handle opened afresh must commit on top of everything before it.
//!
//! The guest kernel makes a volume holding a subvolume `sv`. Through the C
//! ABI only, this crate opens a writable mount and `sv` from it, then:
//!
//! - writes a file through `sv`, and the top level, now stale, refuses;
//! - writes a file through a fresh top-level mount, and `sv`, now stale,
//!   refuses;
//! - writes a file through `sv` opened again from the fresh mount.
//!
//! `dump-super` must show the generation rising by each accepted commit
//! and by nothing else, `btrfs check` must find the volume clean, and the
//! kernel must read every file written through either handle.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_super, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use std::ffi::{c_void, CStr, CString};
use std::path::Path;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

/// The id of the subvolume listed as `name` at the top of `fs`.
fn subvolume_id(fs: *mut fs_btrfs_fs, name: &str) -> u64 {
    let top = cstr("/");
    let iter = unsafe { fs_btrfs_dir_open(fs, top.as_ptr()) };
    assert!(!iter.is_null(), "listing /: {}", last_error());
    let mut found = None;
    loop {
        let entry = unsafe { fs_btrfs_dir_next(iter) };
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        let listed = unsafe { CStr::from_ptr(entry.name.as_ptr()) };
        if listed.to_bytes() == name.as_bytes() && entry.is_subvolume != 0 {
            found = Some(entry.inode);
        }
    }
    unsafe { fs_btrfs_dir_close(iter) };
    found.unwrap_or_else(|| panic!("/ lists no subvolume {name}"))
}

/// The superblock's generation, as `dump-super` reads it.
fn generation(image: &Path) -> u64 {
    let text = dump_super(image);
    text.lines()
        .find_map(|l| {
            let mut words = l.split_whitespace();
            (words.next() == Some("generation")).then(|| words.next().unwrap_or("").to_string())
        })
        .unwrap_or_else(|| panic!("dump-super has no generation:\n{text}"))
        .parse::<u64>()
        .unwrap_or_else(|e| panic!("dump-super's generation is not a number ({e}):\n{text}"))
}

/// Create `path` through `fs` and write `payload` into it: two commits.
fn create_and_write(fs: *mut fs_btrfs_fs, path: &str, payload: &[u8], what: &str) {
    let c_path = cstr(path);
    let ino = unsafe { fs_btrfs_create(fs, c_path.as_ptr(), 0o644) };
    assert_ne!(ino, 0, "create {path} through {what}: {}", last_error());
    let wrote = unsafe {
        fs_btrfs_write_file(
            fs,
            c_path.as_ptr(),
            payload.as_ptr().cast::<c_void>(),
            0,
            payload.len() as u64,
        )
    };
    assert_eq!(
        wrote,
        payload.len() as i64,
        "write {path} through {what}: {}",
        last_error()
    );
}

/// `ESTALE`: 70 on Darwin and 116 on Linux.
const ESTALE: i32 = if cfg!(target_os = "macos") { 70 } else { 116 };

/// A create through `fs` must be refused: its view is older than the volume.
fn assert_stale(fs: *mut fs_btrfs_fs, path: &str, what: &str) {
    let c_path = cstr(path);
    let ino = unsafe { fs_btrfs_create(fs, c_path.as_ptr(), 0o644) };
    assert_eq!(
        ino, 0,
        "{what} committed {path} over a volume another handle had moved on"
    );
    let errno = fs_btrfs_last_errno();
    assert_eq!(
        errno,
        ESTALE,
        "{what} refused {path} with errno {errno} ({}), not ESTALE",
        last_error()
    );
}

#[test]
fn two_handles_of_one_volume_never_commit_over_each_other() {
    let dir = std::path::PathBuf::from(temp_path!("capi-two-handle-commit"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .expect("size the image");
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384"])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "mkfs.btrfs: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    let img = image.to_string_lossy().into_owned();
    guest_kernel_write_ok(
        &img,
        "capi-two-handle-fill",
        r#"
btrfs subvolume create "$MNT/sv" >/dev/null
printf 'kernel in sv\n' > "$MNT/sv/kernel-file"
printf 'kernel at top\n' > "$MNT/top-file"
sync
"#,
    );
    let start = generation(&image);

    let c_img = cstr(&img);
    let top = unsafe { fs_btrfs_mount_rw(c_img.as_ptr()) };
    assert!(!top.is_null(), "mount rw: {}", last_error());
    let id = subvolume_id(top, "sv");
    let sv = unsafe { fs_btrfs_open_subvolume(top, id) };
    assert!(!sv.is_null(), "open sv: {}", last_error());

    create_and_write(sv, "/first", b"first, through sv\n", "sv");
    assert_eq!(generation(&image), start + 2, "after sv's two commits");
    assert_stale(top, "/stale-top", "the top level opened before sv wrote");
    assert_eq!(generation(&image), start + 2, "after the refused top level");
    unsafe { fs_btrfs_umount(top) };

    let top = unsafe { fs_btrfs_mount_rw(c_img.as_ptr()) };
    assert!(!top.is_null(), "mount rw again: {}", last_error());
    create_and_write(top, "/second", b"second, through the top level\n", "top");
    assert_eq!(
        generation(&image),
        start + 4,
        "after the top level's commits"
    );
    assert_stale(sv, "/stale-sv", "sv opened before the top level wrote");
    assert_eq!(generation(&image), start + 4, "after the refused sv");
    unsafe { fs_btrfs_umount(sv) };

    let sv = unsafe { fs_btrfs_open_subvolume(top, id) };
    assert!(!sv.is_null(), "open sv again: {}", last_error());
    create_and_write(sv, "/third", b"third, through sv again\n", "sv");
    assert_eq!(generation(&image), start + 6, "after sv's second commits");
    unsafe { fs_btrfs_umount(sv) };
    unsafe { fs_btrfs_umount(top) };

    assert_btrfs_check_clean(&image, "capi-two-handle-commit");
    let seen = guest_kernel_read_ok(
        &img,
        "capi-two-handle-read",
        r#"
echo top:; ls -A "$MNT"
cat "$MNT/top-file" "$MNT/second"
echo sv:; ls -A "$MNT/sv"
cat "$MNT/sv/kernel-file" "$MNT/sv/first" "$MNT/sv/third"
"#,
    );
    assert_eq!(
        seen,
        "top:\nsecond\nsv\ntop-file\nkernel at top\nsecond, through the top level\n\
         sv:\nfirst\nkernel-file\nthird\n\
         kernel in sv\nfirst, through sv\nthird, through sv again\n",
        "the kernel's view of the volume"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
