//! A change that fills a leaf splits it, rather than refusing the whole
//! transaction (#342). An insert into a full leaf was refused by name,
//! whichever tree the leaf was in: the extent-tree leaf a new block's
//! record lands in, or a directory's leaf in the fs tree. Which leaf is
//! full depends on how the volume happens to be packed, not on what the
//! change asked for, so a write failed or not by luck.
//!
//! The volume here is this crate's own format on a device in memory,
//! with 4 KiB nodes, so a directory fills its fs tree's one leaf after a
//! dozen names. Two hundred names then need that root leaf split, the
//! tree grown to a node over two leaves, and more splits under the node;
//! every name must then read back, before and after a fresh mount. What
//! `btrfs check` and the kernel make of the result is
//! `tests/leaf_split_oracle.rs`.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::inode::FIRST_FREE_OBJECTID;
use fs_core::{BlockDevice, BlockRead};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 256 << 20;
const SECTOR: usize = 512;
const FILES: usize = 200;

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

fn name(i: usize) -> Vec<u8> {
    format!("a-file-with-a-longish-name-{i:04}").into_bytes()
}

/// Every name the directory lists, sorted.
fn listed(fs: &Filesystem) -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = fs
        .read_dir(FIRST_FREE_OBJECTID)
        .expect("list the root directory")
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    names
}

#[test]
fn a_directory_that_fills_its_leaf_goes_on_taking_names() {
    let dev = Arc::new(Sparse {
        sectors: Mutex::new(BTreeMap::new()),
    });
    let opts = fs_btrfs::mkfs::Options {
        nodesize: 4096,
        ..fs_btrfs::mkfs::Options::default()
    };
    fs_btrfs::mkfs::format(dev.as_ref(), &opts).expect("mkfs");

    let mut fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    for i in 0..FILES {
        fs.create(FIRST_FREE_OBJECTID, &name(i), 0o644, 0, 0)
            .unwrap_or_else(|e| panic!("create {} of {FILES}: {e}", i + 1));
    }
    let mut want: Vec<Vec<u8>> = (0..FILES).map(name).collect();
    want.sort();
    assert_eq!(listed(&fs), want, "the names read back in the same mount");
    drop(fs);

    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount again");
    assert_eq!(listed(&fs), want, "the names read back after a fresh mount");
    for i in 0..FILES {
        fs.lookup(FIRST_FREE_OBJECTID, &name(i))
            .unwrap_or_else(|e| panic!("look up {}: {e}", String::from_utf8_lossy(&name(i))));
    }
}
