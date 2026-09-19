#![no_main]
//! A directory item's data: a run of variable-length entries, each
//! declaring its own name length and data length.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_btrfs::dir::parse_dir_items(data);
});
