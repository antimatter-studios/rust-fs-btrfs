//! No-tool contract: a top-level handle another handle has committed past
//! refuses every write as stale, before it reads through its old root tree
//! (#715).
//!
//! The refusal used to come only from the commit, after the write had walked
//! the root tree the stale handle read at mount. Once the commit that replaced
//! that tree has reused its blocks, the walk fails first, with an error that
//! has nothing to do with the cause. So these tests make sure the blocks have
//! been reused, then try each write.

use fs_btrfs::balance::Balance;
use fs_btrfs::capi::*;
use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::mkfs;
use fs_btrfs::subvol::FS_TREE_OBJECTID;
use fs_btrfs::superblock::read_superblock;
use fs_btrfs_test_support::temp_path;
use fs_core::{BlockDevice, FileDevice};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `ESTALE`: 70 on Darwin and 116 on Linux.
const ESTALE: i32 = if cfg!(target_os = "macos") { 70 } else { 116 };

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_add(seed)).collect()
}

/// A scratch directory, removed when it drops, including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(temp_path!("stale-top-level-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        Self(dir)
    }

    /// A formatted image, `name`, in this directory.
    fn volume(&self, name: &str) -> PathBuf {
        let image = self.0.join(name);
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .expect("create the image");
        let dev = FileDevice::open_rw(&image).expect("open rw");
        mkfs::format(&dev, &mkfs::Options::default()).expect("format");
        image
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open rw"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw")
}

/// The generation on the device, read past every handle.
fn generation_on_disk(image: &Path) -> u64 {
    let dev = FileDevice::open(image).expect("open");
    read_superblock(&dev).expect("the superblock").0.generation
}

/// What a stale handle has to work with.
struct Stale {
    fs: Filesystem,
    root: u64,
    file: u64,
    other: u64,
    sub: u64,
    disk: u64,
}

/// A top-level handle two commits behind the disk, through a subvolume
/// handle, on a volume filled as `tests/subvol_snapshot_kernel.rs` fills it.
///
/// Requires the blocks of the root tree the handle read to have been reused:
/// a tree lookup through the handle now fails, which is what every write used
/// to meet before it reached the commit.
fn stale_handle(image: &Path) -> Stale {
    let mut fs = mount_rw(image);
    let root = fs.root_inode().expect("the top directory").ino;
    let small = fs.create(root, b"small", 0o644, 0, 0).unwrap();
    fs.write(small, 0, b"inline bytes").unwrap();
    let file = fs.create(root, b"big", 0o644, 0, 0).unwrap();
    fs.write(file, 0, &pattern(192 << 10, 7)).unwrap();
    let other = fs.create(root, b"other", 0o644, 0, 0).unwrap();
    fs.write(other, 0, &pattern(64 << 10, 99)).unwrap();
    let sub = fs.create_subvolume(root, b"sub", 0o755, 0, 0).unwrap();

    let handle = fs.superblock().generation;
    {
        let mut sv = fs.open_subvolume(sub).unwrap();
        let sv_root = sv.root_inode().unwrap().ino;
        let inside = sv.create(sv_root, b"inside", 0o644, 0, 0).unwrap();
        sv.write(inside, 0, &pattern(80 << 10, 3)).unwrap();
    }

    let disk = generation_on_disk(image);
    assert_eq!(
        fs.superblock().generation,
        handle,
        "the top-level handle still holds the generation it read"
    );
    assert!(
        disk > handle,
        "the subvolume handle's commits moved the disk past {handle}, to {disk}"
    );
    let reused = fs.tree_root_public(FS_TREE_OBJECTID);
    assert!(
        reused.is_err(),
        "the old root tree still resolves ({reused:?}), so its blocks have not been reused \
         and this test proves nothing"
    );
    Stale {
        fs,
        root,
        file,
        other,
        sub,
        disk,
    }
}

#[test]
fn every_top_level_write_refuses_a_stale_handle() {
    let scratch = Scratch::new("every");
    let image = scratch.volume("fs.img");
    let spare = scratch.volume("spare.img");
    let Stale {
        mut fs,
        root,
        file,
        other,
        sub,
        disk,
    } = stale_handle(&image);
    let handle = fs.superblock().generation;
    let spare = || -> Arc<dyn BlockDevice> {
        Arc::new(FileDevice::open_rw(&spare).expect("open the spare device"))
    };
    let mut discards = 0;

    let refusals: Vec<(&str, Option<Error>)> = vec![
        ("create", fs.create(root, b"x", 0o644, 0, 0).err()),
        ("mkdir", fs.mkdir(root, b"x", 0o755, 0, 0).err()),
        ("symlink", fs.symlink(root, b"x", b"big", 0, 0).err()),
        ("link", fs.link(file, root, b"x").err()),
        ("unlink", fs.unlink(root, b"other").err()),
        ("rmdir", fs.rmdir(root, b"small").err()),
        ("rename", fs.rename(root, b"big", root, b"x").err()),
        ("write", fs.write(file, 0, b"x").err()),
        ("write_at", fs.write_at(file, 0, b"x").err()),
        ("truncate", fs.truncate(file, 1).err()),
        ("set_xattr", fs.set_xattr(file, b"user.x", b"y").err()),
        ("remove_xattr", fs.remove_xattr(file, b"user.x").err()),
        ("set_mode", fs.set_mode(file, 0o600).err()),
        ("set_owner", fs.set_owner(file, Some(1), None).err()),
        ("set_times", fs.set_times(file, None, None).err()),
        ("fallocate", fs.fallocate(file, 0, 4096, false).err()),
        ("punch_hole", fs.punch_hole(file, 0, 4096).err()),
        ("clone_range", fs.clone_range(file, 0, other, 0, 4096).err()),
        (
            "dedupe_range",
            fs.dedupe_range(file, 0, other, 0, 4096).err(),
        ),
        ("defragment", fs.defragment(file).err()),
        ("defragment_to", fs.defragment_to(file, 4096).err()),
        (
            "create_subvolume",
            fs.create_subvolume(root, b"x", 0o755, 0, 0).err(),
        ),
        (
            "create_snapshot",
            fs.create_snapshot(FS_TREE_OBJECTID, root, b"x", false)
                .err(),
        ),
        ("delete_subvolume", fs.delete_subvolume(root, b"sub").err()),
        (
            "set_subvolume_read_only",
            fs.set_subvolume_read_only(sub, true).err(),
        ),
        ("set_default_subvolume", fs.set_default_subvolume(sub).err()),
        ("balance", fs.balance(&[] as &[Balance]).err()),
        (
            "relocate_block_group",
            fs.relocate_block_group(0, None).err(),
        ),
        (
            "relocate_block_group_off",
            fs.relocate_block_group_off(0, 1).err(),
        ),
        (
            "copy_tree_blocks_out_of",
            fs.copy_tree_blocks_out_of(0).err(),
        ),
        ("add_device", fs.add_device(spare()).err()),
        ("replace_device", fs.replace_device(1, spare()).err()),
        ("remove_device", fs.remove_device(1).err()),
        ("resize", fs.resize(128 << 20).err()),
        (
            "trim",
            fs.trim(&mut |_| {
                discards += 1;
                Ok(())
            })
            .err(),
        ),
        ("scrub_repair", fs.scrub_repair().err()),
    ];

    let wrong: Vec<String> = refusals
        .iter()
        .filter(|(_, error)| {
            !matches!(error, Some(Error::Stale { handle: h, disk: d }) if *h == handle && *d == disk)
        })
        .map(|(name, error)| format!("{name}: {error:?}"))
        .collect();
    assert!(
        wrong.is_empty(),
        "these did not refuse with Stale {{ handle: {handle}, disk: {disk} }}:\n{}",
        wrong.join("\n")
    );
    assert_eq!(discards, 0, "a stale handle's trim discarded a range");
    assert_eq!(
        generation_on_disk(&image),
        disk,
        "a refused write moved the generation on the device"
    );

    drop(fs);
    let fresh = mount_rw(&image);
    let left = fresh.lookup(fresh.root_inode().unwrap().ino, b"x");
    assert!(
        matches!(left, Err(Error::NotFound)),
        "a refused write left its name behind: {left:?}"
    );
}

#[test]
fn a_stale_handle_opened_again_writes() {
    let scratch = Scratch::new("again");
    let image = scratch.volume("fs.img");
    let Stale { fs, disk, .. } = stale_handle(&image);
    drop(fs);

    let mut fresh = mount_rw(&image);
    let root = fresh.root_inode().unwrap().ino;
    fresh.create(root, b"x", 0o644, 0, 0).unwrap();
    assert_eq!(
        generation_on_disk(&image),
        disk + 1,
        "the handle opened again commits one transaction on top"
    );
}

#[test]
fn the_c_abi_refuses_a_stale_handle_with_estale() {
    let scratch = Scratch::new("capi");
    let image = scratch.volume("fs.img");
    let Stale { fs, sub, disk, .. } = stale_handle(&image);
    drop(fs);

    let path = CString::new(image.to_str().expect("a UTF-8 scratch path")).unwrap();
    let top = unsafe { fs_btrfs_mount_rw(path.as_ptr()) };
    assert!(!top.is_null(), "mount: {}", last_error());
    let sv = unsafe { fs_btrfs_open_subvolume(top, sub) };
    assert!(!sv.is_null(), "open the subvolume: {}", last_error());

    let inside = CString::new("/from-c").unwrap();
    let ino = unsafe { fs_btrfs_create(sv, inside.as_ptr(), 0o644) };
    assert_ne!(ino, 0, "create in the subvolume: {}", last_error());
    let bytes = pattern(80 << 10, 11);
    let written = unsafe {
        fs_btrfs_write_file(
            sv,
            inside.as_ptr(),
            bytes.as_ptr().cast(),
            0,
            bytes.len() as u64,
        )
    };
    assert_eq!(
        written,
        bytes.len() as i64,
        "write in the subvolume: {}",
        last_error()
    );
    let moved = generation_on_disk(&image);
    assert!(
        moved > disk,
        "the subvolume handle's commits moved the disk"
    );

    let name = CString::new("/x").unwrap();
    let ino = unsafe { fs_btrfs_create(top, name.as_ptr(), 0o644) };
    assert_eq!(ino, 0, "a stale handle created a file");
    let errno = fs_btrfs_last_errno();
    assert_eq!(
        errno,
        ESTALE,
        "create refused with errno {errno} ({}), not ESTALE",
        last_error()
    );

    let source = CString::new("/").unwrap();
    let snapshot = CString::new("/snap").unwrap();
    let id = unsafe { fs_btrfs_subvol_snapshot(top, source.as_ptr(), snapshot.as_ptr(), 0) };
    assert_eq!(id, 0, "a stale handle took a snapshot");
    let errno = fs_btrfs_last_errno();
    assert_eq!(
        errno,
        ESTALE,
        "fs_btrfs_subvol_snapshot refused with errno {errno} ({}), not ESTALE",
        last_error()
    );
    assert_eq!(
        generation_on_disk(&image),
        moved,
        "a refused write moved the generation on the device"
    );

    unsafe { fs_btrfs_umount(sv) };
    unsafe { fs_btrfs_umount(top) };
}

#[test]
fn the_c_abi_refuses_a_stale_handle_before_it_resolves_a_path() {
    let scratch = Scratch::new("capi-paths");
    let image = scratch.volume("fs.img");
    let Stale { fs, sub, .. } = stale_handle(&image);
    drop(fs);

    let path = CString::new(image.to_str().expect("a UTF-8 scratch path")).unwrap();
    let top = unsafe { fs_btrfs_mount_rw(path.as_ptr()) };
    assert!(!top.is_null(), "mount: {}", last_error());
    let sv = unsafe { fs_btrfs_open_subvolume(top, sub) };
    assert!(!sv.is_null(), "open the subvolume: {}", last_error());
    let inside = CString::new("/from-c").unwrap();
    let ino = unsafe { fs_btrfs_create(sv, inside.as_ptr(), 0o644) };
    assert_ne!(ino, 0, "create in the subvolume: {}", last_error());
    let moved = generation_on_disk(&image);

    // Each path names nothing, so an entry point that resolves it before it
    // looks at the handle's staleness answers ENOENT.
    let missing = CString::new("/missing/x").unwrap();
    let other = CString::new("/missing/y").unwrap();
    let byte = [0u8];
    let mut answers: Vec<(&str, i32)> = Vec::new();
    let mut ask = |name, refused: bool| {
        assert!(refused, "{name} did not refuse: {}", last_error());
        answers.push((name, fs_btrfs_last_errno()));
    };
    unsafe {
        ask(
            "fs_btrfs_create",
            fs_btrfs_create(top, missing.as_ptr(), 0o644) == 0,
        );
        ask(
            "fs_btrfs_mkdir",
            fs_btrfs_mkdir(top, missing.as_ptr(), 0o755) == 0,
        );
        ask(
            "fs_btrfs_write_file",
            fs_btrfs_write_file(top, missing.as_ptr(), byte.as_ptr().cast(), 0, 1) < 0,
        );
        ask(
            "fs_btrfs_truncate",
            fs_btrfs_truncate(top, missing.as_ptr(), 0) != 0,
        );
        ask(
            "fs_btrfs_rename",
            fs_btrfs_rename(top, missing.as_ptr(), other.as_ptr()) != 0,
        );
        ask(
            "fs_btrfs_subvol_snapshot",
            fs_btrfs_subvol_snapshot(top, missing.as_ptr(), other.as_ptr(), 0) == 0,
        );
        ask(
            "fs_btrfs_subvol_set_readonly",
            fs_btrfs_subvol_set_readonly(top, missing.as_ptr(), 1) != 0,
        );
        ask(
            "fs_btrfs_subvol_set_default",
            fs_btrfs_subvol_set_default(top, missing.as_ptr()) != 0,
        );
    }
    let wrong: Vec<String> = answers
        .iter()
        .filter(|(_, errno)| *errno != ESTALE)
        .map(|(name, errno)| format!("{name}: errno {errno}"))
        .collect();
    assert!(
        wrong.is_empty(),
        "these did not refuse with ESTALE:\n{}",
        wrong.join("\n")
    );
    assert_eq!(
        generation_on_disk(&image),
        moved,
        "a refused write moved the generation on the device"
    );

    unsafe { fs_btrfs_umount(sv) };
    unsafe { fs_btrfs_umount(top) };
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}
