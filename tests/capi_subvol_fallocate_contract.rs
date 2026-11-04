//! Range changes through a selected subvolume's C ABI handle (#735).

use fs_btrfs::capi::*;
use fs_btrfs::{mkfs, Filesystem};
use fs_btrfs_test_support::temp_path;
use fs_core::FileDevice;
use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::Arc;

const SECTOR: u64 = 4096;
const KEEP: u32 = FS_BTRFS_FALLOC_KEEP_SIZE;
const PUNCH: u32 = KEEP | FS_BTRFS_FALLOC_PUNCH_HOLE;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

fn cstr(value: &str) -> CString {
    CString::new(value).unwrap()
}

fn last_error() -> String {
    format!(
        "errno {}: {}",
        fs_btrfs_last_errno(),
        unsafe { CStr::from_ptr(fs_btrfs_last_error()) }.to_string_lossy()
    )
}

struct Handle(*mut fs_btrfs_fs);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { fs_btrfs_umount(self.0) };
    }
}

struct Scratch {
    path: PathBuf,
    subvolume: u64,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = PathBuf::from(temp_path!("capi-subvol-fallocate-{tag}.img"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .and_then(|file| file.set_len(128 << 20))
            .unwrap();
        let device = Arc::new(FileDevice::open_rw(&path).unwrap());
        mkfs::format(device.as_ref(), &mkfs::Options::default()).unwrap();
        let mut top = Filesystem::mount_rw(device).unwrap();
        let root = top.lookup_path("/").unwrap().ino;
        let top_file = top.create(root, b"file", 0o644, 0, 0).unwrap();
        top.write(top_file, 0, &vec![b't'; 4 * SECTOR as usize])
            .unwrap();
        let subvolume = top.create_subvolume(root, b"sv", 0o755, 0, 0).unwrap();
        let mut selected = top.open_subvolume(subvolume).unwrap();
        let file = selected.create(root, b"file", 0o644, 0, 0).unwrap();
        assert_eq!(file, top_file);
        selected
            .write(file, 0, &vec![b's'; 4 * SECTOR as usize])
            .unwrap();
        Self { path, subvolume }
    }

    fn mount(&self, writable: bool) -> Handle {
        let path = cstr(self.path.to_str().unwrap());
        let handle = unsafe {
            if writable {
                fs_btrfs_mount_rw(path.as_ptr())
            } else {
                fs_btrfs_mount(path.as_ptr())
            }
        };
        assert!(!handle.is_null(), "mount: {}", last_error());
        Handle(handle)
    }

    fn selected(&self, top: &Handle) -> Handle {
        let handle = unsafe { fs_btrfs_open_subvolume(top.0, self.subvolume) };
        assert!(!handle.is_null(), "open subvolume: {}", last_error());
        Handle(handle)
    }

    fn verify(&self, expected: &[u8]) {
        let top = self.mount(false);
        assert_eq!(read(&top), vec![b't'; 4 * SECTOR as usize]);
        let selected = self.selected(&top);
        assert_eq!(read(&selected), expected);
        for mode in [0, KEEP, PUNCH] {
            assert_eq!(range(&selected, mode, 0, SECTOR), -1);
            assert_eq!(fs_btrfs_last_errno(), 30, "{}", last_error());
        }
        assert_eq!(read(&selected), expected);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn read(handle: &Handle) -> Vec<u8> {
    let mut bytes = vec![0; 128 << 10];
    let count = unsafe {
        fs_btrfs_read_file(
            handle.0,
            cstr("/file").as_ptr(),
            bytes.as_mut_ptr().cast::<c_void>(),
            0,
            bytes.len() as u64,
        )
    };
    assert!(count >= 0, "read: {}", last_error());
    bytes.truncate(count as usize);
    bytes
}

fn range(handle: &Handle, mode: u32, offset: u64, length: u64) -> i32 {
    unsafe { fs_btrfs_fallocate(handle.0, cstr("/file").as_ptr(), mode, offset, length) }
}

#[test]
fn preallocation_keeps_the_selected_subvolume_across_commits_and_errors() {
    let image = Scratch::new("preallocate");
    let mut expected = vec![b's'; 4 * SECTOR as usize];
    {
        let top = image.mount(true);
        let selected = image.selected(&top);
        assert_eq!(
            range(&selected, KEEP, 8 * SECTOR, 4 * SECTOR),
            0,
            "KEEP_SIZE inside the selected subvolume: {}",
            last_error()
        );
        assert_eq!(read(&selected), expected);
        assert_eq!(range(&selected, 0, 0, 0), -1);
        assert_eq!(fs_btrfs_last_errno(), ENOTSUP, "{}", last_error());
        assert_eq!(
            range(&selected, 0, 4 * SECTOR, 8 * SECTOR),
            0,
            "{}",
            last_error()
        );
        expected.resize(12 * SECTOR as usize, 0);
        assert_eq!(read(&selected), expected);
        assert_eq!(
            range(&selected, PUNCH, 5 * SECTOR, SECTOR),
            0,
            "{}",
            last_error()
        );
        assert_eq!(read(&selected), expected);
    }
    image.verify(&expected);
}

#[test]
fn punching_keeps_the_selected_subvolume_across_whole_and_partial_sectors() {
    let image = Scratch::new("punch");
    let mut expected = vec![b's'; 4 * SECTOR as usize];
    {
        let top = image.mount(true);
        let selected = image.selected(&top);
        assert_eq!(
            range(&selected, PUNCH, SECTOR, 2 * SECTOR),
            0,
            "punch inside the selected subvolume: {}",
            last_error()
        );
        expected[SECTOR as usize..3 * SECTOR as usize].fill(0);
        assert_eq!(read(&selected), expected);
        assert_eq!(range(&selected, PUNCH, 100, 200), 0, "{}", last_error());
        expected[100..300].fill(0);
        assert_eq!(read(&selected), expected);
        assert_eq!(
            range(&selected, PUNCH, 2 * SECTOR, 100),
            0,
            "{}",
            last_error()
        );
        assert_eq!(
            range(&selected, PUNCH, 3 * SECTOR, SECTOR),
            0,
            "{}",
            last_error()
        );
        expected[3 * SECTOR as usize..].fill(0);
        assert_eq!(read(&selected), expected);
    }
    image.verify(&expected);
}
