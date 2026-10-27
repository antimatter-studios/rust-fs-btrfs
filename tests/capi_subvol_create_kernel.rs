//! A subvolume created through the C ABI alone, judged by btrfs-progs and
//! the kernel (#267).
//!
//! On a copy of the kernel-made subvolume fixture, `fs_btrfs_subvol_create`
//! makes `capi-made` at the top and `capi-in-top` inside the ordinary
//! directory `top`. Then:
//!
//! - `btrfs check` must find the volume clean;
//! - the kernel must list both subvolumes with the ids the calls returned
//!   and the parents and paths they were given, and show their modes;
//! - the kernel must write a file inside `capi-made` and read it back,
//!   and the volume must still be clean after.

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

#[test]
fn subvolumes_created_through_the_abi_are_ones_the_kernel_lists_and_uses() {
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "capi-subvol-create-kernel-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("btrfs-subvol.img");
    std::fs::copy(fixture("btrfs-subvol.img"), &image).expect("copy the fixture");
    let img = image.to_str().expect("a UTF-8 scratch path").to_string();

    let (made, in_top) = {
        let c_img = cstr(&img);
        let fs = unsafe { fs_btrfs_mount_rw(c_img.as_ptr()) };
        assert!(!fs.is_null(), "mount rw: {}", last_error());
        let at_top = cstr("/capi-made");
        let made = unsafe { fs_btrfs_subvol_create(fs, at_top.as_ptr(), 0o755) };
        assert_ne!(made, 0, "subvol_create /capi-made: {}", last_error());
        let nested = cstr("/top/capi-in-top");
        let in_top = unsafe { fs_btrfs_subvol_create(fs, nested.as_ptr(), 0o700) };
        assert_ne!(
            in_top,
            0,
            "subvol_create /top/capi-in-top: {}",
            last_error()
        );
        unsafe { fs_btrfs_umount(fs) };
        (made, in_top)
    };
    // The fixture's highest subvolume is 260; each call returns the next.
    assert_eq!((made, in_top), (261, 262), "the returned subvolume ids");

    assert_btrfs_check_clean(&image, "capi-subvol-create");
    let out = guest_kernel_write_ok(
        &img,
        "capi-subvol-create",
        r#"
cd "$MNT"
btrfs subvolume list -p . | awk '{print "list", $2, $6, $NF}'
for s in capi-made top/capi-in-top; do
    echo "id $s $(btrfs inspect-internal rootid "$s")"
    echo "stat $s $(stat -c '%a %i' "$s") $(ls -A "$s" | wc -l)"
done
printf %s 'written by the kernel' > capi-made/k.txt
sync
echo "read $(cat capi-made/k.txt)"
"#,
    );
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    for want in [
        "list 261 5 capi-made",
        "list 262 5 top/capi-in-top",
        "id capi-made 261",
        "id top/capi-in-top 262",
        "stat capi-made 755 256 0",
        "stat top/capi-in-top 700 256 0",
        "read written by the kernel",
    ] {
        assert!(
            lines.contains(&want),
            "kernel output lacks {want:?}:\n{out}"
        );
    }
    assert_btrfs_check_clean(&image, "capi-subvol-create-after-kernel");
    let _ = std::fs::remove_dir_all(&dir);
}
