#![no_main]
//! A whole tree block.
//!
//! The header says how many items the block holds and what level it is
//! at; the items say where their data begins and how long it is, all
//! within a block whose length the decoder does not control. That is
//! three numbers from the image used to index into it.
//!
//! The checksum is re-stamped after mutation -- see
//! `fs_btrfs_fuzz::restamp` for why that is what makes this target
//! test btrfs rather than test crc32c.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let geom = fs_btrfs_fuzz::geometry();
    let mut node = fs_btrfs_fuzz::node(data, geom.nodesize as usize);
    let _ = fs_btrfs::btree::Header::parse(&node);
    fs_btrfs_fuzz::restamp(&mut node);
    let _ = fs_btrfs::btree::Header::parse(&node);
    let at = fs_btrfs_fuzz::logical_of(&node);
    let _ = fs_btrfs::btree::TreeBlock::parse(node, at, geom);
});
