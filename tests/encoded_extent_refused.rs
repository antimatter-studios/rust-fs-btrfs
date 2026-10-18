//! A file extent item that is encrypted or otherwise encoded is refused by
//! name, and the volume's other files still read (#555, part of #450).
//!
//! `decode_extent` refuses an item whose `encryption` byte or
//! `other_encoding` field is non-zero, because this crate decodes neither:
//! reading such an extent as plain data would hand back the stored bytes as
//! if they were the file's. Nothing covered that refusal, so it could drift
//! into a wrong-bytes read with every suite still green.
//!
//! The image is `mkfs.btrfs --rootdir` of two files with regular extents.
//! One file's extent item gets a non-zero `encryption` byte, or separately
//! a non-zero `other_encoding`, in every copy of its leaf, restamped.
//! Reading that file must fail with `UnsupportedFeature` naming the field,
//! and the other file must still read the bytes `mkfs.btrfs` was given.
//! `mkfs.btrfs` runs in the harness VM, the one place this suite has
//! btrfs-progs, so the tool is always there and nothing here is conditional
//! on finding it.

use fs_btrfs::btree::{header_offsets, HEADER_SIZE, ITEM_SIZE};
use fs_btrfs::chunk::objectid;
use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::superblock::Superblock;
use fs_btrfs::tree_write::stamp_checksum;
use fs_btrfs_test_support::{oracle, temp_path};
use fs_core::FileDevice;
use std::sync::Arc;

const SUPERBLOCK: usize = 0x1_0000;
/// `BTRFS_EXTENT_DATA_KEY`.
const EXTENT_DATA: u8 = 108;
// `btrfs_file_extent_item` offsets.
const ENCRYPTION: usize = 17;
const OTHER_ENCODING: usize = 18;

/// Long enough that each file gets a regular extent, not an inline one.
const LEN: usize = 16 * 1024;

/// Incompressible bytes, different for each seed, so a read that returned
/// the wrong file's extent could not match.
fn bytes(seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..LEN)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn target() -> Vec<u8> {
    bytes(0x9E37_79B9_7F4A_7C15)
}

fn other() -> Vec<u8> {
    bytes(0xD1B5_4A32_D192_ED03)
}

/// How the target file's extent item is altered before the read.
#[derive(Clone, Copy)]
enum Encode {
    Nothing,
    Encryption(u8),
    Other(u16),
}

/// An image holding `/target.bin` and `/other.bin`, with the target's extent
/// items altered by `encode`, and the two inode numbers.
///
/// The image and the directory copied into it live under the suite's
/// scratch directory inside this repository, because that is the tree the
/// guest running `mkfs.btrfs` can see.
fn image(name: &str, encode: Encode) -> (std::path::PathBuf, u64, u64) {
    let dir = std::path::PathBuf::from(temp_path!("encoded-extent-{name}"));
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("target.bin"), target()).unwrap();
    std::fs::write(root.join("other.bin"), other()).unwrap();
    let img = dir.join("img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let made = oracle("mkfs.btrfs")
        .arg("-f")
        .arg("--rootdir")
        .arg(&root)
        .arg(&img)
        .output();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );

    let (target_ino, other_ino) = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).unwrap())).unwrap();
        (
            fs.lookup_path("/target.bin").unwrap().ino,
            fs.lookup_path("/other.bin").unwrap().ino,
        )
    };
    if let Encode::Nothing = encode {
        return (img, target_ino, other_ino);
    }

    let mut bytes = std::fs::read(&img).unwrap();
    let sb = Superblock::parse(&bytes[SUPERBLOCK..SUPERBLOCK + 4096]).unwrap();
    let node = sb.nodesize as usize;
    let mut patched = 0;
    let mut at = 0;
    while at + node <= bytes.len() {
        let block = &mut bytes[at..at + node];
        let owner = u64::from_le_bytes(
            block[header_offsets::OWNER..header_offsets::OWNER + 8]
                .try_into()
                .unwrap(),
        );
        if block[header_offsets::FSID..header_offsets::FSID + 16] == sb.fsid
            && owner == objectid::FS_TREE
            && block[header_offsets::LEVEL] == 0
        {
            let nritems = u32::from_le_bytes(
                block[header_offsets::NRITEMS..header_offsets::NRITEMS + 4]
                    .try_into()
                    .unwrap(),
            );
            let mut hit = false;
            for i in 0..nritems as usize {
                let item = HEADER_SIZE + i * ITEM_SIZE;
                if item + ITEM_SIZE > node {
                    break;
                }
                if u64::from_le_bytes(block[item..item + 8].try_into().unwrap()) != target_ino
                    || block[item + 8] != EXTENT_DATA
                {
                    continue;
                }
                let e = HEADER_SIZE
                    + u32::from_le_bytes(block[item + 17..item + 21].try_into().unwrap()) as usize;
                match encode {
                    Encode::Nothing => unreachable!(),
                    Encode::Encryption(v) => block[e + ENCRYPTION] = v,
                    Encode::Other(v) => block[e + OTHER_ENCODING..e + OTHER_ENCODING + 2]
                        .copy_from_slice(&v.to_le_bytes()),
                }
                hit = true;
            }
            if hit {
                stamp_checksum(block, &sb);
                patched += 1;
            }
        }
        at += 4096;
    }
    assert!(
        patched > 0,
        "fixture: the target file's extent item was found"
    );
    std::fs::write(&img, &bytes).unwrap();
    (img, target_ino, other_ino)
}

fn read_file(fs: &Filesystem, ino: u64) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; LEN];
    let n = fs.read_at(ino, 0, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Reads the target, expecting a refusal whose message holds `named`, then
/// reads the other file and expects the bytes `mkfs.btrfs` was given.
fn refused_by_name(name: &str, encode: Encode, named: &str) {
    let (img, target_ino, other_ino) = image(name, encode);
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).unwrap()))
        .expect("an encoded file extent does not stop the volume mounting");
    match read_file(&fs, target_ino) {
        Err(Error::UnsupportedFeature(m)) => {
            assert!(
                m.contains("extent is encoded"),
                "blamed something else: {m}"
            );
            assert!(
                m.contains(named),
                "the refusal does not name {named:?}: {m}"
            );
        }
        other => panic!(
            "expected the encoded extent to be refused, got {:?}",
            other.map(|b| b.len())
        ),
    }
    assert_eq!(
        read_file(&fs, other_ino).expect("the other file still reads"),
        other(),
        "the other file's bytes"
    );
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}

#[test]
fn an_encrypted_file_extent_is_refused_by_name() {
    refused_by_name("encryption", Encode::Encryption(1), "encryption 1");
}

#[test]
fn an_otherwise_encoded_file_extent_is_refused_by_name() {
    refused_by_name("other", Encode::Other(1), "other 1");
}

/// Control: the untouched image reads both files, so the refusals above are
/// the encoding fields' doing and nothing else's.
#[test]
fn an_unencoded_file_extent_reads() {
    let (img, target_ino, other_ino) = image("plain", Encode::Nothing);
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).unwrap())).unwrap();
    assert_eq!(read_file(&fs, target_ino).unwrap(), target());
    assert_eq!(read_file(&fs, other_ino).unwrap(), other());
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}
