//! An fs-verity volume stays writable around its sealed files (#452).
//! The kernel made and sealed the fixture; after our writes, btrfs check
//! judges the volume, and the kernel checks exact payloads and measures
//! every sealed file again with fsverity measure.

use fs_btrfs::{error::Error, fs::Filesystem};
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, fixture, guest_kernel_read_ok, guest_quote, sha256_hex, temp_path,
};
use fs_core::FileDevice;
use std::{path::PathBuf, sync::Arc};

fn refused<T: std::fmt::Debug>(result: fs_btrfs::error::Result<T>) {
    assert!(
        matches!(result, Err(Error::UnsupportedFeature(ref m)) if m.contains("fs-verity")),
        "a sealed file must be refused by name: {result:?}"
    );
}

#[test]
fn unsealed_files_are_written_and_every_sealed_file_keeps_its_kernel_digest() {
    let dir = PathBuf::from(temp_path!("verity-write"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("fs.img");
    std::fs::copy(fixture("features/btrfs-verity.img"), &image).unwrap();
    let text = std::fs::read_to_string(fixture("features/btrfs-verity.manifest")).unwrap();
    let entries: Vec<Vec<&str>> = text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let fields: Vec<_> = l.split('\t').collect();
            assert_eq!(fields.len(), 6, "manifest line {l:?}");
            fields
        })
        .collect();
    let sealed: Vec<_> = entries.iter().filter(|e| e[3] != "-").collect();
    assert_eq!(sealed.len(), 6, "all six sealed files are checked");
    let plain = entries.iter().find(|e| e[3] == "-").expect("unsealed file");
    let plain_size: usize = plain[1].parse().unwrap();
    assert!(plain_size > 0, "the unsealed file has bytes to overwrite");

    let mut fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(&image).unwrap()))
        .expect("mount a verity volume read-write");
    let root = fs.root_inode().unwrap().ino;
    let plain_ino = fs.lookup_path(plain[0]).unwrap().ino;
    let new = fs.create(root, b"verity-new", 0o644, 0, 0).unwrap();
    assert_eq!(fs.write(new, 0, &vec![b'n'; 8192]).unwrap(), 8192);
    assert_eq!(
        fs.write(plain_ino, 0, &vec![b'v'; plain_size]).unwrap(),
        plain_size
    );
    let generation = fs.superblock().generation;

    for e in &sealed {
        let inode = fs.lookup_path(e[0]).unwrap();
        let (parent_path, name) = e[0].rsplit_once('/').unwrap();
        let parent = if parent_path.is_empty() {
            root
        } else {
            fs.lookup_path(parent_path).unwrap().ino
        };
        refused(fs.write(inode.ino, 0, b"changed"));
        refused(fs.write_at(inode.ino, 0, b"changed"));
        refused(fs.truncate(inode.ino, 0));
        refused(fs.truncate(inode.ino, inode.size + 4096));
        refused(fs.truncate(inode.ino, inode.size));
        refused(fs.clone_range(new, 0, inode.ino, 0, 4096));
        refused(fs.unlink(parent, name.as_bytes()));
        refused(fs.rename(root, b"verity-new", parent, name.as_bytes()));
        assert_eq!(
            fs.superblock().generation,
            generation,
            "refusals do not commit"
        );
        assert!(!fs.can_write_in_place(inode.ino).unwrap());
        assert_eq!(sha256_hex(&fs.read_file(inode.ino).unwrap()), e[2]);
        let verity = fs.verity(inode.ino).unwrap().unwrap();
        assert_eq!(verity.measurement(), e[3]);
        assert_eq!(sha256_hex(&verity.descriptor), e[5]);
        assert_eq!(sha256_hex(&fs.verity_merkle_tree(inode.ino).unwrap()), e[4]);
    }
    drop(fs);
    assert_btrfs_check_clean(&image, "unsealed writes on an fs-verity volume");

    let mut script = format!(
        "cd \"$MNT\"\npython3 - <<'PY'\nfrom pathlib import Path\n\
         assert Path('verity-new').read_bytes() == b'n' * 8192\n\
         assert Path({plain:?}).read_bytes() == b'v' * {plain_size}\nPY\n",
        plain = plain[0].trim_start_matches('/'),
    );
    for e in &sealed {
        script.push_str(&format!(
            "fsverity measure \"$MNT\"{} | awk '{{print $1}}'\n",
            guest_quote(e[0])
        ));
    }
    let measurements = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "unsealed payloads and unchanged sealed measurements",
        &script,
    );
    assert_eq!(
        measurements.lines().collect::<Vec<_>>(),
        sealed.iter().map(|e| e[3]).collect::<Vec<_>>()
    );
    std::fs::remove_dir_all(dir).unwrap();
}
