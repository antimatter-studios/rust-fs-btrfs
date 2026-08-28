//! Helpers shared by the oracle tests.
//!
//! Rust builds each file in `tests/` as its own binary, so anything two
//! of them need has to live here and be pulled in with `mod common;`.

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
