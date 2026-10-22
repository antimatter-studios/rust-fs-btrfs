//! Snapshot-shared file extents are expanded to their owning subvolumes as
//! the copy-on-write relocation rewrites each shared leaf (#401).

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = PathBuf::from(temp_path!("relocate-shared-snapshot-data"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "16384"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        Self(dir)
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
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

fn data_groups(fs: &Filesystem) -> Vec<BlockGroup> {
    fs.block_groups()
        .unwrap()
        .into_iter()
        .filter(|g| g.holds_data() && !g.holds_metadata())
        .collect()
}

const CREATE: &str = r#"cd "$MNT"
 btrfs subvolume create source >/dev/null
 head -c 1048576 /dev/urandom > source/file
 sync
 btrfs subvolume snapshot source snapshot >/dev/null
 sync
 for f in source/file snapshot/file; do echo "sum $f $(sha256sum "$f" | cut -d' ' -f1)"; done
"#;

const READ: &str = r#"cd "$MNT"
 for f in source/file snapshot/file; do echo "sum $f $(sha256sum "$f" | cut -d' ' -f1)"; done
"#;

fn manifest(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("sum "))
        .filter_map(|line| line.split_once(' '))
        .map(|(path, sum)| (path.to_string(), sum.to_string()))
        .collect()
}

#[test]
fn snapshot_shared_data_extents_move_and_both_subvolumes_read_their_original_bytes() {
    let scratch = Scratch::new();
    let image = scratch.image();
    let before = guest_kernel_write_ok(
        &image.to_string_lossy(),
        "create and snapshot file data",
        CREATE,
    );
    let before = manifest(&before);
    assert_eq!(before.len(), 2);

    {
        let mut fs = mount_rw(&image);
        let groups = data_groups(&fs);
        assert_eq!(groups.len(), 1, "one data-only group: {groups:?}");
        let old = groups[0];
        let extents = fs
            .block_group_extents(old.start)
            .expect("enumerate snapshot-shared extents");
        assert!(
            extents.iter().any(|extent| {
                extent
                    .items
                    .iter()
                    .map(|item| item.tree)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    > 1
            }),
            "a data extent is named from both subvolumes: {extents:#?}"
        );
        let new = fs
            .relocate_block_group(old.start, None)
            .expect("relocate snapshot-shared data");
        assert_ne!(new, old.start);
    }

    assert_btrfs_check_clean(&image, "after snapshot-shared data relocation");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check: {}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let after = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "read relocated snapshot and source",
        READ,
    );
    assert_eq!(
        manifest(&after),
        before,
        "source or snapshot bytes changed:\n{after}"
    );
}
