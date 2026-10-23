//! A copy-on-write write that starts past a file's end (#433). The kernel
//! zeroes the rest of the old last sector before it extends a file whose
//! end lies partway through one, so the bytes between the old end and the
//! next sector boundary read as zeros whatever the extent held there; the
//! gap between the old end and the write is an implicit hole on a
//! `no-holes` volume and has no item, adding nothing to `nbytes`.
//!
//! The volume here is this crate's own format on a device in memory, and
//! each file is cut short by this crate's truncate, which keeps the extent
//! whole: the bytes past the new end stay on disk, so a write that skipped
//! the zeroing would read them back. What `btrfs check` and the kernel
//! make of the result is `tests/cow_write_oracle.rs`.
//!
//! No fixture, no VM: the unit tier.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::inode::FIRST_FREE_OBJECTID;
use fs_core::{BlockDevice, BlockRead};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 256 << 20;
const SECTOR: usize = 512;

/// A sparse device in memory: only the sectors written are held.
struct Sparse {
    sectors: Mutex<BTreeMap<u64, Box<[u8; SECTOR]>>>,
}

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.sectors.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            match sectors.get(&sector) {
                Some(s) => buf[done..done + n].copy_from_slice(&s[within..within + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        let mut sectors = self.sectors.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            let s = sectors
                .entry(sector)
                .or_insert_with(|| Box::new([0u8; SECTOR]));
            s[within..within + n].copy_from_slice(&buf[done..done + n]);
            done += n;
        }
        Ok(())
    }
    fn flush(&self) -> fs_core::Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A fresh volume of this crate's own format, mounted read-write.
fn volume() -> (Arc<Sparse>, Filesystem) {
    let dev = Arc::new(Sparse {
        sectors: Mutex::new(BTreeMap::new()),
    });
    fs_btrfs::mkfs::format(dev.as_ref(), &fs_btrfs::mkfs::Options::default()).expect("mkfs");
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    (dev, fs)
}

/// `name`: two sectors of 0x11, then cut to 5000 bytes by this crate's
/// truncate, so 0x11 stays on disk from byte 5000 to the end of the second
/// sector.
fn stale_tail(fs: &mut Filesystem, name: &[u8]) -> u64 {
    let ino = fs
        .create(FIRST_FREE_OBJECTID, name, 0o644, 0, 0)
        .expect("create");
    fs.write(ino, 0, &[0x11; 8192]).expect("write two sectors");
    fs.truncate(ino, 5000).expect("truncate to 5000");
    let inode = fs.read_inode(ino).unwrap();
    assert_eq!((inode.size, inode.nbytes), (5000, 8192), "the cut file");
    ino
}

/// What a file reads as after `write` lands at `offset` past an end at 5000.
fn expected(offset: usize, write: &[u8]) -> Vec<u8> {
    let mut want = vec![0x11; 5000];
    want.resize(offset, 0);
    want.extend_from_slice(write);
    want
}

#[test]
fn a_write_sectors_past_the_end_zeroes_the_old_tail_and_leaves_the_gap_a_hole() {
    let (dev, mut fs) = volume();
    let ino = stale_tail(&mut fs, b"gap");
    let write = [0xa5u8; 3000];
    let n = fs
        .write(ino, 20_000, &write)
        .expect("a write that starts sectors past the end");
    assert_eq!(n, write.len());
    let want = expected(20_000, &write);
    assert_eq!(
        fs.read_file(ino).unwrap(),
        want,
        "the old tail and the gap read as zeros, then the write"
    );
    let inode = fs.read_inode(ino).unwrap();
    // The old two sectors, copied, and the two the write touches; the
    // three sectors of the gap between them hold no extent.
    assert_eq!(
        (inode.size, inode.nbytes),
        (23_000, 16_384),
        "size and nbytes"
    );
    drop(fs);

    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount again");
    assert_eq!(fs.read_file(ino).unwrap(), want, "after a fresh mount");
}

#[test]
fn a_write_inside_the_old_last_sector_past_the_end_zeroes_up_to_it() {
    let (dev, mut fs) = volume();
    let ino = stale_tail(&mut fs, b"near");
    let write = [0x5au8; 1000];
    fs.write(ino, 6000, &write)
        .expect("a write that starts past the end, inside the old last sector");
    let want = expected(6000, &write);
    assert_eq!(
        fs.read_file(ino).unwrap(),
        want,
        "the bytes between the old end and the write read as zeros"
    );
    let inode = fs.read_inode(ino).unwrap();
    assert_eq!(
        (inode.size, inode.nbytes),
        (7000, 8192),
        "no sector is added inside the old last one"
    );
    drop(fs);

    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount again");
    assert_eq!(fs.read_file(ino).unwrap(), want, "after a fresh mount");
}

#[test]
fn a_write_past_an_end_on_a_sector_boundary_adds_only_its_own_sectors() {
    let (_dev, mut fs) = volume();
    let ino = fs
        .create(FIRST_FREE_OBJECTID, b"aligned", 0o644, 0, 0)
        .expect("create");
    fs.write(ino, 0, &[0x22; 4096]).expect("write one sector");
    fs.write(ino, 3 * 4096, &[0x33; 100])
        .expect("a write two sectors past an aligned end");
    let mut want = vec![0x22; 4096];
    want.resize(3 * 4096, 0);
    want.extend_from_slice(&[0x33; 100]);
    assert_eq!(fs.read_file(ino).unwrap(), want);
    let inode = fs.read_inode(ino).unwrap();
    assert_eq!((inode.size, inode.nbytes), (3 * 4096 + 100, 2 * 4096));
}
