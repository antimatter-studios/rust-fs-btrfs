//! Namespace changes and truncate inside a subvolume other than the top
//! level, made through the C ABI alone and judged by btrfs-progs and the
//! kernel (#520).
//!
//! The guest kernel makes a volume holding a subvolume `sv` with files in
//! it and a file and a directory at the top. Then, through C ABI calls
//! only, this crate opens a writable mount, finds `sv` in its listing,
//! opens it with `fs_btrfs_open_subvolume`, and creates, writes, links,
//! renames, unlinks, removes a directory and truncates inside it. Then:
//!
//! - `btrfs check` must find the volume clean;
//! - the kernel must mount `sv` alone (`-o subvol=sv`), list what was
//!   written and read it back byte for byte;
//! - the kernel must list the top-level subvolume exactly as it left it.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use std::ffi::{c_void, CStr, CString};

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

/// One C ABI call returning 0 on success, checked with the ABI's error.
fn ok(what: &str, rc: i32) {
    assert_eq!(rc, 0, "{what}: {}", last_error());
}

#[test]
fn changes_made_through_the_abi_inside_a_subvolume_are_clean_and_the_kernel_reads_them() {
    let dir = std::path::PathBuf::from(temp_path!("capi-subvol-write-kernel"));
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
        "capi-subvol-write-fill",
        r#"
btrfs subvolume create "$MNT/sv" >/dev/null
printf 'linked payload\n' > "$MNT/sv/source"
printf 'renamed payload\n' > "$MNT/sv/old"
printf 'abcdefgh%.0s' $(seq 8192) > "$MNT/sv/big"
mkdir "$MNT/sv/empty"
printf 'held at top\n' > "$MNT/top-file"
mkdir "$MNT/top-dir"
sync
"#,
    );

    {
        let c_img = cstr(&img);
        let top = unsafe { fs_btrfs_mount_rw(c_img.as_ptr()) };
        assert!(!top.is_null(), "mount rw: {}", last_error());
        let id = subvolume_id(top, "sv");
        let sv = unsafe { fs_btrfs_open_subvolume(top, id) };
        assert!(!sv.is_null(), "open sv: {}", last_error());

        let made = cstr("/made");
        let ino = unsafe { fs_btrfs_create(sv, made.as_ptr(), 0o644) };
        assert_ne!(ino, 0, "create inside sv: {}", last_error());
        let payload = b"made through the ABI\n";
        let wrote = unsafe {
            fs_btrfs_write_file(
                sv,
                made.as_ptr(),
                payload.as_ptr().cast::<c_void>(),
                0,
                payload.len() as u64,
            )
        };
        assert_eq!(
            wrote,
            payload.len() as i64,
            "write inside sv: {}",
            last_error()
        );
        let (source, linked) = (cstr("/source"), cstr("/linked"));
        ok("link inside sv", unsafe {
            fs_btrfs_link(sv, source.as_ptr(), linked.as_ptr())
        });
        let (old, renamed) = (cstr("/old"), cstr("/renamed"));
        ok("rename inside sv", unsafe {
            fs_btrfs_rename(sv, old.as_ptr(), renamed.as_ptr())
        });
        ok("unlink one of two names inside sv", unsafe {
            fs_btrfs_unlink(sv, source.as_ptr())
        });
        let empty = cstr("/empty");
        ok("rmdir inside sv", unsafe {
            fs_btrfs_rmdir(sv, empty.as_ptr())
        });
        let made_dir = cstr("/made-dir");
        let dino = unsafe { fs_btrfs_mkdir(sv, made_dir.as_ptr(), 0o755) };
        assert_ne!(dino, 0, "mkdir inside sv: {}", last_error());
        let big = cstr("/big");
        ok("truncate inside sv", unsafe {
            fs_btrfs_truncate(sv, big.as_ptr(), 4096)
        });
        unsafe { fs_btrfs_umount(sv) };
        unsafe { fs_btrfs_umount(top) };
    }

    assert_btrfs_check_clean(&image, "capi-subvol-write");
    let seen = guest_kernel_read_ok(
        &img,
        "capi-subvol-write-read",
        r#"
src=$(findmnt -no SOURCE "$MNT")
mkdir -p /tmp/capi-sv
mount -o ro,subvol=sv "$src" /tmp/capi-sv
echo names:; ls -A /tmp/capi-sv
echo made:; cat /tmp/capi-sv/made
echo linked:; cat /tmp/capi-sv/linked
echo links:; stat -c %h /tmp/capi-sv/linked
echo renamed:; cat /tmp/capi-sv/renamed
echo big:; stat -c %s /tmp/capi-sv/big; head -c 16 /tmp/capi-sv/big; echo
echo dir:; stat -c %F /tmp/capi-sv/made-dir
umount /tmp/capi-sv
echo top:; ls -A "$MNT"
cat "$MNT/top-file"
test -z "$(ls -A "$MNT/top-dir")" && echo top-dir-empty
"#,
    );
    assert_eq!(
        seen,
        "names:\nbig\nlinked\nmade\nmade-dir\nrenamed\n\
         made:\nmade through the ABI\n\
         linked:\nlinked payload\nlinks:\n1\n\
         renamed:\nrenamed payload\n\
         big:\n4096\nabcdefghabcdefgh\n\
         dir:\ndirectory\n\
         top:\nsv\ntop-dir\ntop-file\nheld at top\ntop-dir-empty\n",
        "the kernel's view of the volume"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
