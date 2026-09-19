#![no_main]
//! An xattr item's data. Same shape as a directory item and a different
//! decoder, which is the sort of near-duplicate where one of the two
//! gets a bound and the other does not.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_btrfs::xattr::parse_xattr_items(data);
});
