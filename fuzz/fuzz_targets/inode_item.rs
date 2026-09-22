#![no_main]
//! One inode item's data, as it appears in a leaf.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_btrfs::inode::Inode::parse(data, 256);
});
