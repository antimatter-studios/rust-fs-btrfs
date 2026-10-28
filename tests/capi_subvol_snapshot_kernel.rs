//! Snapshots taken through the C ABI alone, judged by btrfs-progs and the
//! kernel (#267).
//!
//! On a copy of the kernel-made subvolume fixture, `fs_btrfs_subvol_snapshot`
//! snapshots the subvolume `sub` as `capi-snap` at the top, and the top
//! level as the read-only `capi-rosnap` inside the ordinary directory
//! `top`. Then:
//!
//! - `btrfs check` must find the volume clean;
//! - the kernel must list both with `btrfs subvolume list -s`, with the
//!   ids the calls returned, and `btrfs subvolume show` must give each its
//!   source's UUID as its parent UUID, and `capi-rosnap` the read-only
//!   flag;
//! - each snapshot must hold its source's files as the kernel wrote them;
//! - the kernel must write into `capi-snap` and be refused a write into
//!   `capi-rosnap`, and the volume must still be clean after.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{assert_btrfs_check_clean, fixture, guest_kernel_write_ok};
use std::ffi::{CStr, CString};

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

/// The value after `field:` on the line of `out` that starts with
/// `prefix` and holds it.
fn field<'a>(out: &'a str, prefix: &str, field: &str) -> &'a str {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix(prefix))
        .find_map(|l| l.trim().strip_prefix(field))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no {prefix}{field} line in the kernel's output:\n{out}"))
}

#[test]
fn snapshots_taken_through_the_abi_are_ones_the_kernel_lists_and_uses() {
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "capi-subvol-snapshot-kernel-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("btrfs-subvol.img");
    std::fs::copy(fixture("btrfs-subvol.img"), &image).expect("copy the fixture");
    let img = image.to_str().expect("a UTF-8 scratch path").to_string();

    let (snap, rosnap) = {
        let c_img = cstr(&img);
        let fs = unsafe { fs_btrfs_mount_rw(c_img.as_ptr()) };
        assert!(!fs.is_null(), "mount rw: {}", last_error());
        let (sub, at_top) = (cstr("/sub"), cstr("/capi-snap"));
        let snap = unsafe { fs_btrfs_subvol_snapshot(fs, sub.as_ptr(), at_top.as_ptr(), 0) };
        assert_ne!(snap, 0, "snapshot /sub as /capi-snap: {}", last_error());
        let (top, in_top) = (cstr("/"), cstr("/top/capi-rosnap"));
        let rosnap = unsafe { fs_btrfs_subvol_snapshot(fs, top.as_ptr(), in_top.as_ptr(), 1) };
        assert_ne!(
            rosnap,
            0,
            "snapshot / as /top/capi-rosnap: {}",
            last_error()
        );
        unsafe { fs_btrfs_umount(fs) };
        (snap, rosnap)
    };
    // The fixture's highest subvolume is 260; each call returns the next.
    assert_eq!((snap, rosnap), (261, 262), "the returned snapshot ids");

    assert_btrfs_check_clean(&image, "capi-subvol-snapshot");
    let out = guest_kernel_write_ok(
        &img,
        "capi-subvol-snapshot",
        r#"
cd "$MNT"
btrfs subvolume list -s . | awk '{print "list", $2, $NF}'
btrfs subvolume show . | sed 's/^/top /'
btrfs subvolume show sub | sed 's/^/sub /'
btrfs subvolume show capi-snap | sed 's/^/snap /'
btrfs subvolume show top/capi-rosnap | sed 's/^/rosnap /'
btrfs property get -ts top/capi-rosnap ro | sed 's/^/rosnap-prop /'
echo "read snap $(cat capi-snap/b.txt) $(cat capi-snap/after.txt)"
echo "read rosnap $(cat top/capi-rosnap/top/a.txt)"
printf %s 'written by the kernel' > capi-snap/k.txt
sync
echo "wrote $(cat capi-snap/k.txt)"
if printf 'x' > top/capi-rosnap/new 2>/dev/null; then echo rosnap-written; else echo rosnap-refused; fi
"#,
    );
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    for want in [
        "list 261 capi-snap",
        "list 262 top/capi-rosnap",
        "read snap in sub added after the snapshot",
        "read rosnap in the default subvolume",
        "wrote written by the kernel",
        "rosnap-refused",
    ] {
        assert!(
            lines.contains(&want),
            "kernel output lacks {want:?}:\n{out}"
        );
    }
    let top_uuid = field(&out, "top ", "UUID:");
    let sub_uuid = field(&out, "sub ", "UUID:");
    assert_eq!(field(&out, "snap ", "Parent UUID:"), sub_uuid, "{out}");
    assert_eq!(field(&out, "rosnap ", "Parent UUID:"), top_uuid, "{out}");
    assert_ne!(field(&out, "snap ", "UUID:"), sub_uuid, "{out}");
    assert_eq!(field(&out, "rosnap-prop ", "ro="), "true", "{out}");
    assert_btrfs_check_clean(&image, "capi-subvol-snapshot-after-kernel");
    let _ = std::fs::remove_dir_all(&dir);
}
