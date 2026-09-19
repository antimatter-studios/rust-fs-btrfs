#![no_main]
//! The superblock, read before anything is known. The node size, the
//! sector size and the checksum type all come out of it, and the system
//! chunk array behind it is what maps the first logical address to a
//! physical one.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_btrfs::Superblock::parse(data);
    // The mirrors live at fixed offsets, and a mount reads them when the
    // primary does not parse.
    let _ = fs_btrfs::superblock::Superblock::parse_at(data, 0x10000);
});
