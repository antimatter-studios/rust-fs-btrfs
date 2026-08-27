//! Building tree blocks.
//!
//! Btrfs is copy-on-write: nothing is modified where it lies. Changing
//! one byte of one inode means writing a new leaf holding the change, a
//! new node above it pointing at that leaf, and so on to the root — then
//! a new superblock naming the new root. This builds those blocks.
//!
//! It writes nothing. A block is bytes, and where they go and in what
//! order is the transaction's business — which matters more here than in
//! most filesystems, because the ordering *is* the crash-consistency.
//! `docs/transaction-format.md` records the order, now observed rather
//! than reasoned: tree blocks, flush, superblocks, flush.
//!
//! # The shape of a leaf
//!
//! ```text
//!   0  header       101 bytes: checksum, filesystem UUID, its own
//!                   address, flags, chunk-tree UUID, generation, owner,
//!                   item count, level
//! 101  items        25 bytes each, growing forwards
//!  ...  free space
//!  ...  item data, growing backwards from the end of the block
//! ```
//!
//! Items grow forwards and their data grows backwards, so the free space
//! is in the middle — the same arrangement an XFS block-form directory
//! uses, and for the same reason: neither end has to move when the other
//! grows.
//!
//! # Three things that are easy to get wrong
//!
//! **An item's offset is measured from the end of the header**, not from
//! the start of the block. A block-relative offset is wrong by 101 and
//! lands inside the previous item.
//!
//! **The checksum does not cover itself.** It is the first 32 bytes, and
//! it is computed over everything after them — so it can only be
//! computed once the rest of the block is final.
//!
//! **A block records its own address.** A tree block copied to a
//! different place and not re-stamped fails its own identity check, and
//! the reader here rejects it — which is the behaviour that catches a
//! copy-on-write writer that forgot the block moved.

use crate::btree::{header_offsets as o, HEADER_SIZE, ITEM_SIZE, LEAF_DATA_OFFSET};
use crate::chunk::{DiskKey, DISK_KEY_SIZE};
use crate::error::{Error, Result};
use crate::superblock::{Superblock, CSUM_SIZE};

/// One item to place in a leaf: where it is filed, and its body.
#[derive(Debug, Clone)]
pub struct LeafItem<'a> {
    pub key: DiskKey,
    pub data: &'a [u8],
}

/// What a leaf's items and their data occupy, header included.
///
/// Used to refuse a leaf that will not fit rather than build one whose
/// item array has run into its own data.
pub fn space_needed(items: &[LeafItem]) -> usize {
    HEADER_SIZE
        + items
            .iter()
            .map(|i| ITEM_SIZE + i.data.len())
            .sum::<usize>()
}

/// Where a block belongs and what it says about itself.
///
/// Grouped rather than passed as five arguments because they are all
/// answers to "which block is this", and getting one wrong produces a
/// block that parses and is rejected on the next read.
#[derive(Debug, Clone, Copy)]
pub struct BlockIdentity {
    /// The logical address the block will live at. A block records this
    /// so a reader can tell it was found where it claims to be.
    pub bytenr: u64,
    /// The tree this block belongs to — `FS_TREE`, the root tree, and so
    /// on.
    pub owner: u64,
    /// The transaction writing it.
    pub generation: u64,
    /// Zero for a leaf; the height above the leaves otherwise.
    pub level: u8,
    /// The header flags — see [`flags_for_new_block`]. Not derivable
    /// from the rest, and zero is wrong on every modern filesystem.
    pub flags: u64,
    /// The chunk tree's UUID, which every tree block carries.
    ///
    /// It is a constant of the filesystem and is not in the superblock's
    /// parsed fields, so it is taken from a block already on disk rather
    /// than invented — see [`chunk_tree_uuid_of`].
    pub chunk_tree_uuid: [u8; 16],
}

/// The flags a block written now should carry.
///
/// Two things share the word. `WRITTEN` says the block has been written
/// out at least once, which is true of anything this produces. The top
/// byte is the back-reference revision, and every filesystem with the
/// `MIXED_BACKREF` feature — which is every modern one — uses revision
/// 1.
///
/// Zero is wrong, and wrong in a way nothing here would catch: a block
/// with revision 0 claims a back-reference format that predates the
/// feature, and the kernel reads its references accordingly.
///
/// This was not reasoned. Rebuilding the kernel's own leaves showed
/// `0x0100000000000001` where a zeroed field had been assumed, and the
/// two halves of that value are exactly the two constants below.
pub fn flags_for_new_block() -> u64 {
    use crate::btree::header_flags::{MIXED_BACKREF_REV, WRITTEN};
    WRITTEN | ((MIXED_BACKREF_REV as u64) << crate::btree::header_flags::BACKREF_REV_SHIFT)
}

/// The chunk-tree UUID a tree block carries.
///
/// Every block of a filesystem carries the same one, so reading it off
/// any block gives the value a new block must repeat. Copying it is not
/// laziness: a block whose chunk-tree UUID does not match is a block
/// from another filesystem, which is exactly what the field is for.
pub fn chunk_tree_uuid_of(block: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&block[o::CHUNK_TREE_UUID..o::CHUNK_TREE_UUID + 16]);
    out
}

/// Build a leaf block.
///
/// `items` must be sorted by key, which is what every search of the tree
/// relies on and what nothing downstream re-checks.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] if the items do not fit in one block —
/// splitting a leaf is the caller's decision, not this function's — and
/// if they are not sorted.
pub fn build_leaf(sb: &Superblock, id: BlockIdentity, items: &[LeafItem]) -> Result<Vec<u8>> {
    let nodesize = sb.nodesize as usize;

    let needed = space_needed(items);
    if needed > nodesize {
        return Err(Error::UnsupportedFeature(format!(
            "{} items need {needed} bytes and a tree block holds {nodesize}; splitting a \
             leaf is not implemented",
            items.len()
        )));
    }

    // Sorted by key is not a preference. A B-tree search bisects on it,
    // so an unsorted leaf is not slow, it is wrong — and it is wrong in
    // a way that finds *some* items and silently misses others.
    for pair in items.windows(2) {
        if !key_before(&pair[0].key, &pair[1].key) {
            return Err(Error::UnsupportedFeature(format!(
                "leaf items are out of order: {:?} is not before {:?}",
                pair[0].key, pair[1].key
            )));
        }
    }

    let mut block = vec![0u8; nodesize];
    write_header(&mut block, sb, id, items.len() as u32);

    // Items forwards from the header, their data backwards from the end.
    let mut data_end = nodesize;
    for (i, item) in items.iter().enumerate() {
        let at = LEAF_DATA_OFFSET + i * ITEM_SIZE;
        data_end -= item.data.len();

        write_key(&mut block[at..], &item.key);
        // An item's offset is from the END OF THE HEADER, not from the
        // start of the block.
        let offset = (data_end - LEAF_DATA_OFFSET) as u32;
        let k = at + DISK_KEY_SIZE;
        block[k..k + 4].copy_from_slice(&offset.to_le_bytes());
        block[k + 4..k + 8].copy_from_slice(&(item.data.len() as u32).to_le_bytes());

        block[data_end..data_end + item.data.len()].copy_from_slice(item.data);
    }

    stamp_checksum(&mut block, sb);
    Ok(block)
}

/// Write the 101-byte header every tree block opens with.
///
/// The checksum is left for [`stamp_checksum`], which has to run last
/// because it covers everything after itself.
fn write_header(block: &mut [u8], sb: &Superblock, id: BlockIdentity, nritems: u32) {
    block[o::FSID..o::FSID + 16].copy_from_slice(&sb.fsid);
    block[o::BYTENR..o::BYTENR + 8].copy_from_slice(&id.bytenr.to_le_bytes());
    block[o::FLAGS..o::FLAGS + 8].copy_from_slice(&id.flags.to_le_bytes());
    block[o::CHUNK_TREE_UUID..o::CHUNK_TREE_UUID + 16].copy_from_slice(&id.chunk_tree_uuid);
    block[o::GENERATION..o::GENERATION + 8].copy_from_slice(&id.generation.to_le_bytes());
    block[o::OWNER..o::OWNER + 8].copy_from_slice(&id.owner.to_le_bytes());
    block[o::NRITEMS..o::NRITEMS + 4].copy_from_slice(&nritems.to_le_bytes());
    block[o::LEVEL] = id.level;
}

/// A key, in the 17 bytes the on-disk form uses.
fn write_key(out: &mut [u8], key: &DiskKey) {
    out[0..8].copy_from_slice(&key.objectid.to_le_bytes());
    out[8] = key.key_type;
    out[9..17].copy_from_slice(&key.offset.to_le_bytes());
}

/// Compute and store the block's checksum.
///
/// It covers everything after itself — the first [`CSUM_SIZE`] bytes are
/// the digest, and the digest is of the rest. So this has to be the last
/// thing done to a block, and re-stamping after any later edit is not
/// optional.
pub fn stamp_checksum(block: &mut [u8], sb: &Superblock) {
    let digest = sb.csum_type.digest(&block[CSUM_SIZE..]);
    block[..CSUM_SIZE].copy_from_slice(&digest);
}

/// Whether `a` sorts strictly before `b`, by the ordering the tree is
/// built on: objectid, then type, then offset.
fn key_before(a: &DiskKey, b: &DiskKey) -> bool {
    (a.objectid, a.key_type, a.offset) < (b.objectid, b.key_type, b.offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(objectid: u64, key_type: u8, offset: u64) -> DiskKey {
        DiskKey {
            objectid,
            key_type,
            offset,
        }
    }

    /// The ordering a search bisects on, including the two fields people
    /// forget are part of it.
    #[test]
    fn keys_order_by_all_three_fields() {
        assert!(key_before(&key(1, 0, 0), &key(2, 0, 0)), "objectid first");
        assert!(key_before(&key(1, 1, 0), &key(1, 2, 0)), "then type");
        assert!(key_before(&key(1, 1, 1), &key(1, 1, 2)), "then offset");

        // A larger objectid wins even when the type is smaller, which is
        // the case an ordering written field-by-field gets wrong.
        assert!(key_before(&key(1, 99, 0), &key(2, 0, 0)));
        assert!(!key_before(&key(1, 0, 0), &key(1, 0, 0)), "strictly before");
    }

    /// Space is the header, then a fixed cost per item plus its data.
    #[test]
    fn space_is_the_header_plus_each_item_and_its_body() {
        let a = [1u8; 10];
        let b = [2u8; 20];
        let items = [
            LeafItem {
                key: key(1, 1, 0),
                data: &a,
            },
            LeafItem {
                key: key(2, 1, 0),
                data: &b,
            },
        ];
        assert_eq!(space_needed(&items), HEADER_SIZE + 2 * ITEM_SIZE + 10 + 20);
        assert_eq!(
            space_needed(&[]),
            HEADER_SIZE,
            "an empty leaf is its header"
        );
    }
}
