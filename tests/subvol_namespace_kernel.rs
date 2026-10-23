//! Names made inside a subvolume other than the top level, judged by
//! btrfs-progs and the kernel (#517).
//!
//! The guest kernel makes a volume holding a subvolume `sv` with a file
//! in it and a file at the top. This crate opens `sv` on a writable mount
//! and makes a directory, a regular file and a symbolic link inside it,
//! each one committed transaction against that subvolume's tree. Then:
//!
//! - `btrfs check` must find the volume clean;
//! - the kernel must list the three new names inside `sv`, next to the
//!   file it made there, and read that file's bytes unchanged;
//! - the kernel must list the top-level subvolume exactly as it left it.

use std::sync::Arc;

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};

/// What the kernel writes before this crate touches the volume.
const FILL: &str = "btrfs subvolume create \"$MNT/sv\" >/dev/null\n\
     printf 'held in sv\\n' > \"$MNT/sv/kept\"\n\
     printf 'held at top\\n' > \"$MNT/top-file\"\n\
     mkdir \"$MNT/top-dir\"\n\
     sync";

/// The kernel's view of the top level and of `sv`.
const LIST: &str = "echo top:; ls -A \"$MNT\" | grep -v '^sv$'\n\
     echo sv:; ls -A \"$MNT/sv\"\n\
     echo kept:; cat \"$MNT/sv/kept\"\n\
     echo dir:; stat -c '%F' \"$MNT/sv/made-dir\"\n\
     echo link:; readlink \"$MNT/sv/made-link\"";

#[test]
fn names_made_inside_a_subvolume_are_clean_and_the_kernel_lists_them() {
    let dir = std::path::PathBuf::from(temp_path!("subvol-namespace-kernel"));
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
    guest_kernel_write_ok(&img, "subvol-namespace-fill", FILL);

    {
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open rw"));
        let top = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
        let id = top.subvolume_at(b"/sv").expect("find sv");
        let mut fs = top.open_subvolume(id).expect("open sv");
        let root = fs.lookup_path("/").expect("sv's top directory").ino;
        let made_dir = fs
            .mkdir(root, b"made-dir", 0o755, 0, 0)
            .expect("mkdir inside sv");
        fs.create(made_dir, b"inner", 0o644, 0, 0)
            .expect("create inside sv's new directory");
        fs.create(root, b"made-file", 0o644, 0, 0)
            .expect("create inside sv");
        fs.symlink(root, b"made-link", b"kept", 0, 0)
            .expect("symlink inside sv");
        assert_eq!(
            fs.subvolume_at(b"/").expect("the mount's own subvolume"),
            id,
            "after its commits the mount is still on sv"
        );
    }

    assert_btrfs_check_clean(&image, "subvol-namespace");
    let seen = guest_kernel_read_ok(&img, "subvol-namespace-list", LIST);
    assert_eq!(
        seen,
        "top:\ntop-dir\ntop-file\n\
         sv:\nkept\nmade-dir\nmade-file\nmade-link\n\
         kept:\nheld in sv\n\
         dir:\ndirectory\n\
         link:\nkept\n",
        "the kernel's view of the volume"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Both metadata-only changes and the removal of regular data extents
/// must name sv's tree, including a replacing rename (#518).
#[test]
fn names_changed_inside_a_subvolume_are_clean_and_the_kernel_reads_them() {
    let dir = std::path::PathBuf::from(temp_path!("subvol-namespace-changes"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .unwrap();
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384"])
        .arg(&image)
        .output();
    assert!(made.status.success(), "{:?}", made.stderr);
    let img = image.to_string_lossy().into_owned();
    guest_kernel_write_ok(
        &img,
        "subvol-namespace-changes-fill",
        r#"
btrfs subvolume create "$MNT/sv" >/dev/null
printf 'linked payload\n' > "$MNT/sv/source"
printf 'renamed payload\n' > "$MNT/sv/old"
printf 'abcdefgh%.0s' $(seq 8192) > "$MNT/sv/delete"
cp --reflink=never "$MNT/sv/delete" "$MNT/sv/replace"
mkdir "$MNT/sv/empty" "$MNT/sv/left" "$MNT/sv/right" "$MNT/sv/left/moved"
printf 'nested payload\n' > "$MNT/sv/left/moved/inner"
printf 'held at top\n' > "$MNT/top-file"
mkdir "$MNT/top-dir"
sync
"#,
    );
    {
        let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
        let top = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap();
        let id = top.subvolume_at(b"/sv").unwrap();
        let mut fs = top.open_subvolume(id).unwrap();
        let root = fs.lookup_path("/").unwrap().ino;
        let source = fs.lookup_path("/source").unwrap().ino;
        fs.link(source, root, b"linked").expect("link inside sv");
        fs.rename(root, b"old", root, b"renamed")
            .expect("rename inside sv");
        fs.unlink(root, b"source").expect("unlink one of two names");
        fs.unlink(root, b"delete").expect("release data inside sv");
        fs.rename(root, b"renamed", root, b"replace")
            .expect("replace a file holding data inside sv");
        fs.rmdir(root, b"empty").expect("rmdir inside sv");
        let left = fs.lookup_path("/left").unwrap().ino;
        let right = fs.lookup_path("/right").unwrap().ino;
        fs.rename(left, b"moved", right, b"moved")
            .expect("move a directory between parents inside sv");
        assert_eq!(fs.subvolume_at(b"/").unwrap(), id);
        assert_eq!(fs.read_file(source).unwrap(), b"linked payload\n");
    }
    assert_btrfs_check_clean(&image, "subvol-namespace-changes");
    let seen = guest_kernel_read_ok(
        &img,
        "subvol-namespace-changes-read",
        r#"
src=$(findmnt -no SOURCE "$MNT")
mkdir -p /tmp/sv-namespace-only
mount -o ro,subvol=sv "$src" /tmp/sv-namespace-only
echo names:
ls -A /tmp/sv-namespace-only
echo linked:; cat /tmp/sv-namespace-only/linked
echo links:; stat -c %h /tmp/sv-namespace-only/linked
echo renamed:; cat /tmp/sv-namespace-only/replace
echo nested:; cat /tmp/sv-namespace-only/right/moved/inner
test -z "$(ls -A /tmp/sv-namespace-only/left)"
umount /tmp/sv-namespace-only
echo top:; ls -A "$MNT"
cat "$MNT/top-file"
test -z "$(ls -A "$MNT/top-dir")"
"#,
    );
    assert_eq!(
        seen,
        "names:\nleft\nlinked\nreplace\nright\n\
         linked:\nlinked payload\nlinks:\n1\n\
         renamed:\nrenamed payload\nnested:\nnested payload\n\
         top:\nsv\ntop-dir\ntop-file\nheld at top\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
