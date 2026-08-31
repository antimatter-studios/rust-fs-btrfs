//! Helpers shared by the oracle tests.
//!
//! Rust builds each file in `tests/` as its own binary, so anything two
//! of them need has to live here and be pulled in with `mod common;`.
//!
//! That also means this module is compiled once *per* binary, and each
//! one uses only the part it needs — so an unused helper here is the
//! normal case, not a leftover.

#![allow(dead_code)]

use std::path::Path;

/// Whether an image's superblock says the filesystem spans more than one
/// device.
///
/// Most oracles here read a logical address straight out of a flat
/// image, or require every image to mount. Neither holds for one member
/// of a multi-device filesystem: its chunks may live on the other disk,
/// so a flat read returns whatever lies at that offset — bytes that
/// parse and then fail a checksum against a block they were never meant
/// to be — and mounting it is refused on purpose.
///
/// So a pool member is not a fixture for those tests, and this is how
/// they tell. `tests/pool_oracle.rs` is where such an image IS the
/// subject.
///
/// Read from the raw bytes rather than through the parser, because the
/// point is to decide whether to involve the parser at all.
pub fn spans_several_devices(image: &Path) -> bool {
    /// `num_devices`, at 0x88 within the superblock at 64 KiB.
    const NUM_DEVICES: usize = 0x1_0000 + 0x88;
    std::fs::read(image)
        .ok()
        .filter(|b| b.len() >= NUM_DEVICES + 8)
        .map(|b| u64::from_le_bytes(b[NUM_DEVICES..NUM_DEVICES + 8].try_into().unwrap()))
        .is_some_and(|devices| devices > 1)
}

/// Little-endian `u32` at `at`.
///
/// Hand-rolled here, and **deliberately not** `src/`'s reader. These
/// oracles decode leaves by hand on purpose: one that read a leaf
/// through `btree::TreeBlock` would be checking the writer against the
/// reader rather than against the disk, and the two agreeing is exactly
/// what an oracle must not assume.
///
/// So this moves the decoder *sideways* — out of eight test binaries
/// and into the module they already share — without moving it into the
/// crate under test. That keeps the independence and removes seven
/// copies.
///
/// # Panics
///
/// If `at + 4` is past the end. A fixture that is too short is a broken
/// test, not a case to handle.
pub fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes in range"))
}

/// Little-endian `u64` at `at`. See [`le32`] on why this is not the
/// crate's own reader.
///
/// # Panics
///
/// If `at + 8` is past the end.
pub fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes in range"))
}
