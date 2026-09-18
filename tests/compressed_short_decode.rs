//! A compressed stream that decodes short is refused anywhere but the
//! file's last extent (#189).
//!
//! `decompress` pads a short decode with zeros up to `ram_bytes`. That is
//! right for the last extent of a file, where the tail of the final sector
//! holds nothing. Anywhere else it turns a truncated or damaged stream into
//! a run of zeros — which is legitimate file content, so a caller cannot
//! tell it from data. Every other decode failure in that module is
//! reported; this one was not, and it is the one that hides damage.
//!
//! The image is a file of 256 KiB written by the kernel under
//! `compress-force=lzo`, so it holds several compressed extents. The first
//! The first extent's LZO stream then has its length header lowered so it
//! ends after one segment: the decoder finishes cleanly and hands back
//! 4 KiB where the item says 128 KiB, which is what a truncated or damaged
//! stream looks like from the reader's side. Truncating the *input* instead
//! is caught by the framing — this is the case that is not.
//!
//! The inode is marked `nodatasum` for the same reason the write-side
//! fixtures do: the checksums cover the bytes on disk, and this edits
//! them. Skips without btrfs-progs or a kernel that can mount btrfs, unless
//! `BTRFS_ORACLE_FIXTURES=required`.

use fs_btrfs::btree::{header_offsets, HEADER_SIZE, ITEM_SIZE};
use fs_btrfs::chunk::objectid;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::superblock::Superblock;
use fs_btrfs::tree_write::stamp_checksum;
use fs_btrfs::write::{INODE_NODATACOW, INODE_NODATASUM};
use fs_core::{BlockRead, FileDevice};
use std::process::Command;
use std::sync::Arc;

const SUPERBLOCK: usize = 0x1_0000;
/// Two compression units, so the first extent is not the last.
const LEN: usize = 256 * 1024;
const DISK_BYTENR: usize = 21;
const INODE_FLAGS: usize = 64;
const INODE_ITEM_KEY: u8 = 1;
/// The LZO framing's first four bytes: how much of the extent is stream.
const LZO_LEN: usize = 4;
const COMPRESSION: usize = 16;
const EXTENT_DATA_KEY: u8 = 108;

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Ask the kernel for a compressed file, which is the only way to get one:
/// `mkfs.btrfs --rootdir` does not compress.
fn image(name: &str) -> Option<std::path::PathBuf> {
    let dir =
        std::env::temp_dir().join(format!("btrfs-short-decode-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(400 * 1024 * 1024)
        .unwrap();
    // Compressible enough that LZO keeps it — btrfs stores an extent
    // uncompressed when compressing it barely helps — and varied enough
    // that a run of zeros in the result is visible as wrong. Each line
    // carries its own number, so every byte of the file says where it
    // belongs.
    let body: Vec<u8> = {
        let mut out: Vec<u8> = Vec::with_capacity(LEN + 64);
        let mut line = 0u32;
        while out.len() < LEN {
            out.extend_from_slice(
                format!("the quick brown fox jumps over the lazy dog {line:08}\n").as_bytes(),
            );
            line += 1;
        }
        out.truncate(LEN);
        out
    };
    std::fs::write(dir.join("body"), &body).unwrap();
    let script = format!(
        r#"
        mkfs.btrfs -f {img} > /dev/null 2>&1 || exit 10
        m=$(mktemp -d)
        mount -o loop,compress-force=lzo {img} "$m" || exit 11
        cp {body} "$m/file.bin"
        sync
        umount "$m"
        rmdir "$m"
        chmod 666 {img}
        "#,
        img = img.display(),
        body = dir.join("body").display()
    );
    let out = Command::new("sudo")
        .args([
            "-n",
            "env",
            &format!("PATH={}", std::env::var("PATH").unwrap_or_default()),
            "bash",
            "-c",
            &script,
        ])
        .output();
    let ok = matches!(&out, Ok(o) if o.status.success());
    if !ok {
        assert!(
            std::env::var("BTRFS_ORACLE_FIXTURES").as_deref() != Ok("required"),
            "BTRFS_ORACLE_FIXTURES=required, but the fixture could not be built: {out:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    Some(img)
}

/// Apply `edit(key_type, key_offset, body)` to `ino`'s items in every
/// fs-tree leaf copy, and say how many leaves were touched.
fn edit_items(
    img: &std::path::Path,
    ino: u64,
    mut edit: impl FnMut(u8, u64, &mut [u8]) -> bool,
) -> usize {
    let mut bytes = std::fs::read(img).unwrap();
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
            if le64(block, item) != ino {
                continue;
            }
            let key_type = block[item + 8];
            let key_offset = le64(block, item + 9);
            let off = HEADER_SIZE
                + u32::from_le_bytes(block[item + 17..item + 21].try_into().unwrap()) as usize;
            let size = u32::from_le_bytes(block[item + 21..item + 25].try_into().unwrap()) as usize;
            hit |= edit(key_type, key_offset, &mut block[off..off + size]);
        }
        if hit {
            stamp_checksum(block, &sb);
            patched += 1;
        }
    }
    std::fs::write(img, &bytes).unwrap();
    patched
}

fn mount(img: &std::path::Path) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open(img).unwrap()) as Arc<dyn BlockRead>).unwrap()
}

/// Control: the file as the kernel wrote it reads back, and it really is
/// compressed and in more than one extent — otherwise the test below is
/// about nothing.
#[test]
fn the_compressed_file_as_made_reads_back() {
    let Some(img) = image("control") else {
        eprintln!("no kernel or btrfs-progs -- skipping");
        return;
    };
    let fs = mount(&img);
    let ino = fs.lookup_path("/file.bin").unwrap().ino;
    let mut compressed = 0usize;
    edit_items(&img, ino, |key_type, _, body| {
        if key_type == EXTENT_DATA_KEY && body[COMPRESSION] != 0 {
            compressed += 1;
        }
        false
    });
    assert!(
        compressed >= 2,
        "the fixture holds {compressed} compressed extents, so none of them is a \
         non-final one"
    );
    let read = mount(&img).read_file(ino).expect("an ordinary read");
    assert_eq!(read.len(), LEN);
    assert_eq!(
        read,
        std::fs::read(img.parent().unwrap().join("body")).unwrap(),
        "the file did not read back what was written to it"
    );
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}

#[test]
fn a_short_decode_in_a_non_final_extent_is_refused() {
    let Some(img) = image("short") else {
        eprintln!("no kernel or btrfs-progs -- skipping");
        return;
    };
    let ino = mount(&img).lookup_path("/file.bin").unwrap().ino;
    // The FIRST extent, which is not the file's last.
    let mut first_extent = 0u64;
    let patched = edit_items(&img, ino, |key_type, key_offset, body| match key_type {
        // The checksums cover the bytes this edits, so the inode says not
        // to check them — as the write-side fixtures do.
        INODE_ITEM_KEY => {
            let flags = le64(body, INODE_FLAGS);
            body[INODE_FLAGS..INODE_FLAGS + 8]
                .copy_from_slice(&(flags | INODE_NODATACOW | INODE_NODATASUM).to_le_bytes());
            true
        }
        EXTENT_DATA_KEY if key_offset == 0 && body[COMPRESSION] != 0 => {
            first_extent = le64(body, DISK_BYTENR);
            false
        }
        _ => false,
    });
    assert!(patched >= 1, "fixture: the file's inode was found");
    assert_ne!(
        first_extent, 0,
        "fixture: the file's first extent was found"
    );

    // The stream's own length header, lowered to one segment: the decoder
    // stops there, cleanly, with a fraction of the bytes the item promises.
    {
        let physical = mount(&img)
            .chunk_map()
            .map(first_extent)
            .expect("the extent's address")
            .physical;
        let mut bytes = std::fs::read(&img).unwrap();
        let at = physical as usize;
        let first_segment =
            u32::from_le_bytes(bytes[at + LZO_LEN..at + LZO_LEN + 4].try_into().unwrap()) as usize;
        let shortened = (LZO_LEN + LZO_LEN + first_segment) as u32;
        bytes[at..at + LZO_LEN].copy_from_slice(&shortened.to_le_bytes());
        std::fs::write(&img, &bytes).unwrap();
    }

    match mount(&img).read_file(ino) {
        Err(e) => {
            let said = e.to_string();
            assert!(
                said.contains("decoded"),
                "refused, but not for the short decode: {said}"
            );
        }
        Ok(read) => {
            let zeros = read.iter().filter(|b| **b == 0).count();
            panic!(
                "read {} bytes through a stream that decodes short, {zeros} of them zeros",
                read.len()
            );
        }
    }
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}
