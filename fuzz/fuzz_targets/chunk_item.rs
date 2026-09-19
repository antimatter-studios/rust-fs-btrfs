#![no_main]
//! A chunk item: the logical-to-physical mapping itself, with a stripe
//! count that says how many stripe records follow it.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_btrfs::Chunk::parse(0, data);
    let _ = fs_btrfs::chunk::Stripe::parse(data);
    let _ = fs_btrfs::chunk::DiskKey::parse(data);
});
