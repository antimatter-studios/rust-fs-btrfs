//! A whole-file read of an inode claiming an impossible size fails with an
//! error, not an abort (#80).
//!
//! `read_file` bounded `inode.size` only by the volume's `total_bytes` and
//! then allocated it. On a large volume that is no bound at all. An
//! allocation that cannot be satisfied aborts the process, and
//! `capi::guard`'s `catch_unwind` cannot turn an abort into an errno. Here
//! the file's inode item claims 2^60 bytes and the superblock claims a
//! volume large enough to allow it.
//!
//! The image is `mkfs.btrfs --rootdir` of one small file, with the inode
//! item patched in every copy of its leaf and the primary superblock
//! patched, all restamped. Skips without btrfs-progs, unless
//! `BTRFS_ORACLE_FIXTURES=required`.

use fs_btrfs::btree::{header_offsets, HEADER_SIZE, ITEM_SIZE};
use fs_btrfs::chunk::objectid;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::superblock::{offsets, Superblock};
use fs_core::{BlockRead, FileDevice};
use std::process::Command;
use std::sync::Arc;

const SUPERBLOCK: usize = 0x1_0000;
/// `btrfs_inode_item.size`.
const INODE_SIZE: usize = 16;
const CLAIMED: u64 = 1 << 60;

fn image() -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("btrfs-huge-size-{}", std::process::id()));
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("file.bin"), b"a small file").unwrap();
    let img = dir.join("img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let made = match Command::new("mkfs.btrfs")
        .arg("-f")
        .arg("--rootdir")
        .arg(&root)
        .arg(&img)
        .output()
    {
        Ok(made) => made,
        Err(e) => {
            assert!(
                std::env::var("BTRFS_ORACLE_FIXTURES").as_deref() != Ok("required"),
                "BTRFS_ORACLE_FIXTURES=required, but mkfs.btrfs is not runnable: {e}"
            );
            return None;
        }
    };
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    Some(img)
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[test]
fn an_impossible_file_size_is_an_error_not_an_abort() {
    let Some(img) = image() else {
        eprintln!("no mkfs.btrfs -- skipping");
        return;
    };
    let ino = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).unwrap()) as Arc<dyn BlockRead>)
            .unwrap();
        let ino = fs.lookup_path("/file.bin").unwrap().ino;
        assert_eq!(
            fs.read_path("/file.bin").unwrap(),
            b"a small file",
            "control"
        );
        ino
    };

    let mut bytes = std::fs::read(&img).unwrap();
    let sb = Superblock::parse(&bytes[SUPERBLOCK..SUPERBLOCK + 4096]).unwrap();
    let node = sb.nodesize as usize;
    let mut patched = 0;
    for at in (0..bytes.len() - node).step_by(4096) {
        let block = &mut bytes[at..at + node];
        if block[header_offsets::FSID..header_offsets::FSID + 16] != sb.fsid
            || le64(block, header_offsets::OWNER) != objectid::FS_TREE
            || block[header_offsets::LEVEL] != 0
        {
            continue;
        }
        let nritems = u32::from_le_bytes(
            block[header_offsets::NRITEMS..header_offsets::NRITEMS + 4]
                .try_into()
                .unwrap(),
        );
        let mut hit = false;
        for i in 0..nritems as usize {
            let item = HEADER_SIZE + i * ITEM_SIZE;
            if le64(block, item) == ino && block[item + 8] == 1 {
                let off = HEADER_SIZE
                    + u32::from_le_bytes(block[item + 17..item + 21].try_into().unwrap()) as usize;
                block[off + INODE_SIZE..off + INODE_SIZE + 8]
                    .copy_from_slice(&CLAIMED.to_le_bytes());
                hit = true;
            }
        }
        if hit {
            fs_btrfs::tree_write::stamp_checksum(block, &sb);
            patched += 1;
        }
    }
    assert!(patched >= 1, "fixture: the inode item was not found");
    let super_block = &mut bytes[SUPERBLOCK..SUPERBLOCK + 4096];
    super_block[offsets::TOTAL_BYTES..offsets::TOTAL_BYTES + 8]
        .copy_from_slice(&(1u64 << 62).to_le_bytes());
    fs_btrfs::super_write::stamp_checksum(super_block, sb.csum_type);
    std::fs::write(&img, &bytes).unwrap();

    let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).unwrap()) as Arc<dyn BlockRead>)
        .expect("the patched volume still mounts");
    assert_eq!(
        fs.read_inode(ino).unwrap().size,
        CLAIMED,
        "fixture: the size patch took"
    );
    match fs.read_file(ino) {
        Err(e) => assert!(
            format!("{e}").contains("read_at"),
            "refused, but without saying what to use: {e}"
        ),
        Ok(_) => panic!("2^60 bytes were read into memory"),
    }
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}
