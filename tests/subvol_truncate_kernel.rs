//! A chosen subvolume's truncates preserve its kernel-made snapshot (#519).

use fs_btrfs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path,
};
use fs_core::FileDevice;
use std::{path::PathBuf, sync::Arc};

#[test]
fn subvolume_truncates_preserve_the_snapshot_and_top_level_for_the_kernel() {
    let dir = PathBuf::from(temp_path!("subvol-truncate-kernel"));
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
    let img = image.to_string_lossy();
    guest_kernel_write_ok(
        &img,
        "subvol-truncate-fill",
        r#"
btrfs subvolume create "$MNT/sv" >/dev/null
printf 'abcdefgh%.0s' $(seq 8192) > "$MNT/sv/data"
cp --reflink=never "$MNT/sv/data" "$MNT/sv/removed"
mkdir "$MNT/sv/nc"
chattr +C "$MNT/sv/nc"
cp --reflink=never "$MNT/sv/data" "$MNT/sv/nc/data"
for n in $(seq 400); do touch "$MNT/sv/pad-$n"; done
printf 'held at top\n' > "$MNT/top-file"
sync
btrfs subvolume snapshot -r "$MNT/sv" "$MNT/snap" >/dev/null
sync
"#,
    );
    {
        let top = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        let subs = top.subvolumes().unwrap();
        let sv = subs.iter().find(|s| s.path == "sv").unwrap();
        assert!(sv.last_snapshot > 0);
        let top_sv = subs.iter().find(|s| s.id == 5).unwrap();
        assert_eq!(top_sv.last_snapshot, 0, "only sv was snapshotted");
        let mut fs = top.open_subvolume(sv.id).unwrap();
        let nc_inode = fs.lookup_path("/nc/data").unwrap();
        let flags = fs_btrfs::inode::INODE_NODATACOW | fs_btrfs::inode::INODE_NODATASUM;
        assert_eq!(
            nc_inode.flags & flags,
            flags,
            "nc/data can otherwise use nocow"
        );
        let nc = nc_inode.ino;
        // A tall snapshotted tree still records one data reference. This
        // makes last_snapshot, rather than refs > 1, the decisive guard.
        let tree = dump_tree(&image, &sv.id.to_string());
        assert!(
            tree.contains("level 1"),
            "the test needs shared child leaves"
        );
        let marker = format!("key ({nc} EXTENT_DATA 0)");
        let item = tree.split(&marker).nth(1).expect("nc extent item");
        let disk_line = item.lines().find(|l| l.contains("disk byte ")).unwrap();
        let bytenr = disk_line
            .split("disk byte ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        let extents = dump_tree(&image, "extent");
        let marker = format!("key ({bytenr} EXTENT_ITEM ");
        let record = extents.split(&marker).nth(1).expect("nc extent record");
        assert!(record.lines().nth(1).unwrap().contains("refs 1 "));
        assert!(
            !fs.can_write_in_place(nc).unwrap(),
            "sv's snapshot shares nc/data"
        );

        let ino = fs.lookup_path("/data").unwrap().ino;
        fs.truncate(ino, 5000).expect("shrink inside sv");
        fs.truncate(ino, 12000)
            .expect("grow and zero the last partial sector");
        let removed = fs.lookup_path("/removed").unwrap().ino;
        fs.truncate(removed, 0)
            .expect("drop sv's shared data reference");
        assert_eq!(fs.subvolume_at(b"/").unwrap(), sv.id);
        let fresh = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        let fresh_top = fresh
            .subvolumes()
            .unwrap()
            .into_iter()
            .find(|s| s.id == 5)
            .unwrap();
        assert_eq!(fresh_top.bytenr, top_sv.bytenr, "tree 5 was not rewritten");
        let ro = fresh.open_subvolume(sv.id).unwrap();
        assert!(
            !ro.can_write_in_place(nc).unwrap(),
            "a read-only handle sees the same snapshot"
        );
    }
    assert_btrfs_check_clean(&image, "subvol-truncate");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert!(
        checked.status.success(),
        "{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let seen = guest_kernel_read_ok(
        &img,
        "subvol-truncate-read",
        r#"
src=$(findmnt -no SOURCE "$MNT")
mkdir -p /tmp/sv-truncate-only /tmp/sv-truncate-snapshot
mount -o ro,subvol=sv "$src" /tmp/sv-truncate-only
mount -o ro,subvol=snap "$src" /tmp/sv-truncate-snapshot
echo size:; stat -c %s /tmp/sv-truncate-only/data
echo file:; sha256sum < /tmp/sv-truncate-only/data
echo removed:; stat -c %s /tmp/sv-truncate-only/removed
echo snapshot:; sha256sum < /tmp/sv-truncate-snapshot/data
echo snapshot-removed:; sha256sum < /tmp/sv-truncate-snapshot/removed
echo snapshot-nc:; sha256sum < /tmp/sv-truncate-snapshot/nc/data
echo top:; ls -A "$MNT"; cat "$MNT/top-file"
umount /tmp/sv-truncate-snapshot /tmp/sv-truncate-only
"#,
    );
    let original = b"abcdefgh".repeat(8192);
    let mut want = original[..5000].to_vec();
    want.resize(12000, 0);
    let hash = sha256_hex(&original);
    assert_eq!(
        seen,
        format!(
            "size:\n12000\nfile:\n{}  -\nremoved:\n0\nsnapshot:\n{hash}  -\n\
         snapshot-removed:\n{hash}  -\nsnapshot-nc:\n{hash}  -\n\
         top:\nsnap\nsv\ntop-file\nheld at top\n",
            sha256_hex(&want)
        )
    );
    let _ = std::fs::remove_dir_all(&dir);
}
