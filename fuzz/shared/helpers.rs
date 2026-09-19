// Shared by both tiers, included textually rather than depended on.
//
// `tests/fuzz_decoders.rs` and `fuzz/src/lib.rs` both `include!` this
// file. A crate dependency would have been tidier, but the fuzz crate
// depends on `libfuzzer-sys`, which builds libFuzzer's C++ runtime, and
// making the gate depend on the fuzz crate would drag that into every
// pull request build on the stable toolchain.
//
// What matters is that the two tiers prepare a block identically -- the
// same fixed superblock, the same length normalisation, the same
// checksum re-stamp. If they did not, a reproducer from one would not
// reproduce in the other.

use fs_btrfs::Superblock;

/// The superblock of the filesystem the corpus was cut from.
///
/// Read at runtime rather than with `include_bytes!`, because this file
/// is `include!`d into two crates whose manifest directories differ --
/// a relative path that is right for one is wrong for the other, and
/// silently so. Each tier defines `corpus_root()`; that is the one
/// thing they do not share.
pub fn superblock() -> &'static Superblock {
    static SB: OnceLock<Superblock> = OnceLock::new();
    SB.get_or_init(|| {
        let path = corpus_root().join("superblock/plain.bin");
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!("reading the superblock seed {}: {e}", path.display())
        });
        Superblock::parse(&bytes).expect("the committed superblock seed parses")
    })
}

/// The tree geometry that superblock describes.
pub fn geometry() -> &'static fs_btrfs::btree::TreeGeometry {
    static GEOM: OnceLock<fs_btrfs::btree::TreeGeometry> = OnceLock::new();
    GEOM.get_or_init(|| fs_btrfs::btree::TreeGeometry::from_superblock(superblock()))
}

/// Re-stamp a block's checksum so the decoder gets past the gate at its
/// front door.
///
/// THIS IS THE DIFFERENCE BETWEEN FUZZING btrfs AND FUZZING crc32c.
/// `TreeBlock::parse` verifies the checksum before it looks at anything
/// else, so a mutated block is rejected on the first line and the item
/// walk -- the part with the arithmetic in it -- is never reached. A
/// fuzzer left like that would spend its whole budget proving that a
/// checksum check works.
///
/// A crafted image has a *valid* checksum. Whoever wrote it computed
/// one, because they wanted the block to be read. So re-stamping is not
/// a cheat that weakens the test; it is what makes the test resemble
/// the threat.
///
/// btrfs checksums cover everything after the 32-byte checksum field.
pub fn restamp(block: &mut [u8]) {
    if block.len() <= CSUM_SIZE {
        return;
    }
    let digest = superblock().csum_type.digest(&block[CSUM_SIZE..]);
    let len = superblock().csum_type.digest_len().min(CSUM_SIZE);
    block[..len].copy_from_slice(&digest[..len]);
}

/// The logical address a block says it lives at.
///
/// `TreeBlock::parse` refuses a block whose recorded `bytenr` is not
/// the address it was read from -- that is how a block proves it
/// belongs where it was found, and it is a second gate in front of the
/// item walk, after the checksum. Reading the address out of the block
/// keeps the two consistent, which is what a crafted image would do:
/// whoever wrote it chose both numbers.
pub fn logical_of(block: &[u8]) -> u64 {
    if block.len() < 56 {
        return 0;
    }
    u64::from_le_bytes(block[48..56].try_into().expect("8 bytes"))
}

/// The width of the checksum field at the front of every block.
pub const CSUM_SIZE: usize = 32;

/// Present the fuzzer's bytes as a node of exactly `len`.
///
/// A device hands back a whole node, so that is what these decoders are
/// called with in the real path. Left to itself libFuzzer would spend
/// most of its budget on lengths no image can produce.
///
/// Short input repeats rather than being zero-padded: a block of zeros
/// fails the fsid check on the first line, and the point is to get
/// further in than that.
pub fn node(data: &[u8], len: usize) -> Vec<u8> {
    if data.is_empty() {
        return vec![0; len];
    }
    data.iter().copied().cycle().take(len).collect()
}
