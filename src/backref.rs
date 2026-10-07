//! The back references an extent record carries, and moving the parent
//! one of them names (#287).
//!
//! An extent item says who refers to the extent. Each reference is either
//! KEYED — "tree 5 refers to this", or for data "inode 588 of tree 5, at
//! file offset 0" — or SHARED, naming the address of the tree block that
//! holds the pointer: its parent. Which one a block's children get is the
//! block's own choice, recorded in its extent item as `FULL_BACKREF`.
//! Balance leaves blocks flagged that way, because it relocates through a
//! tree that is not the one the blocks finally belong to.
//!
//! A copy-on-write move gives a block a new address, so a shared
//! reference naming the old one names a block that is gone. The move
//! keeps the block's flag and re-points each such reference at the new
//! address. A re-pointed reference is the same size it was, so no leaf
//! grows, which is what makes this the cheaper of the two consistent
//! answers; the other is converting every child to a keyed reference,
//! sixteen bytes larger per data extent.
//!
//! References live inline in the extent item when there is room, as a
//! type byte followed by a payload, and otherwise as items of their own
//! keyed `(extent, type, parent)`. Inline ones are ordered by type and
//! then by their offset field, which for a shared reference is the
//! parent; re-pointing one can move it among its neighbours.

use crate::chunk::key_type;
use crate::error::{Error, Result};

/// `BTRFS_BLOCK_FLAG_FULL_BACKREF`: the block's children are referenced
/// by its address rather than by the tree that owns it.
pub const BLOCK_FLAG_FULL_BACKREF: u64 = 1 << 8;

/// `BTRFS_EXTENT_OWNER_REF_KEY`, as an inline reference type.
pub const EXTENT_OWNER_REF: u8 = 172;
/// `BTRFS_TREE_BLOCK_REF_KEY`.
pub const TREE_BLOCK_REF: u8 = crate::extent_write::TREE_BLOCK_REF;
/// `BTRFS_EXTENT_DATA_REF_KEY`.
pub const EXTENT_DATA_REF: u8 = 178;
/// `BTRFS_SHARED_BLOCK_REF_KEY`.
pub const SHARED_BLOCK_REF: u8 = 182;
/// `BTRFS_SHARED_DATA_REF_KEY`.
pub const SHARED_DATA_REF: u8 = 184;

/// The fixed part of an extent item: refs, generation, flags.
const ITEM_HEADER: usize = 24;
/// `btrfs_tree_block_info`: a key and a level, between the flags and the
/// first reference of a tree block's `EXTENT_ITEM` (never of a skinny
/// `METADATA_ITEM`).
const TREE_BLOCK_INFO: usize = 18;
/// `flags`, within the fixed part.
const FLAGS: usize = 16;

/// One inline reference: its type byte and the payload after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineRef {
    /// The reference type, one of the `*_REF` constants.
    pub kind: u8,
    /// Everything after the type byte.
    pub payload: Vec<u8>,
}

impl InlineRef {
    /// The first eight payload bytes: the owning tree, the parent, or for
    /// a keyed data reference the tree its inode is in.
    pub fn offset(&self) -> u64 {
        u64::from_le_bytes(
            self.payload[..8]
                .try_into()
                .expect("every payload has 8 bytes"),
        )
    }

    /// The key inline references are ordered by. A keyed data reference
    /// is ordered by a hash of its fields; those are never re-pointed, so
    /// they keep the order they were found in rather than this code
    /// computing a hash it has no other use for.
    fn order(&self) -> (u8, u64) {
        if self.kind == EXTENT_DATA_REF {
            (self.kind, 0)
        } else {
            (self.kind, self.offset())
        }
    }
}

/// How long an inline reference's payload is.
fn payload_len(kind: u8) -> Option<usize> {
    match kind {
        EXTENT_OWNER_REF | TREE_BLOCK_REF | SHARED_BLOCK_REF => Some(8),
        // root, objectid, offset, count
        EXTENT_DATA_REF => Some(28),
        // parent, count
        SHARED_DATA_REF => Some(12),
        _ => None,
    }
}

/// An extent item's flags, or `None` when it is too short to have any.
pub fn flags(body: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(
        body.get(FLAGS..FLAGS + 8)?.try_into().ok()?,
    ))
}

/// Where the inline references start in an item filed under `item_type`.
fn refs_start(item_type: u8, body: &[u8]) -> Result<usize> {
    let flags = flags(body).ok_or_else(|| short(body.len()))?;
    let tree_block = flags & crate::extent_write::EXTENT_FLAG_TREE_BLOCK != 0;
    Ok(if item_type == key_type::EXTENT_ITEM && tree_block {
        ITEM_HEADER + TREE_BLOCK_INFO
    } else {
        ITEM_HEADER
    })
}

fn short(len: usize) -> Error {
    Error::UnsupportedFeature(format!(
        "an extent item of {len} bytes is shorter than its own fixed fields"
    ))
}

/// The inline references of an extent item filed under `item_type`
/// (`EXTENT_ITEM` or `METADATA_ITEM`).
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] for a reference type this does not know
/// the size of, or a body that ends inside a reference: either way the
/// rest of the item cannot be read, and guessing would rewrite it wrong.
pub fn inline_refs(item_type: u8, body: &[u8]) -> Result<Vec<InlineRef>> {
    let mut at = refs_start(item_type, body)?;
    if at > body.len() {
        return Err(short(body.len()));
    }
    let mut out = Vec::new();
    while at < body.len() {
        let kind = body[at];
        let len = payload_len(kind).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "an extent item carries an inline reference of type {kind}, whose size \
                 this driver does not know"
            ))
        })?;
        let payload = body.get(at + 1..at + 1 + len).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "an extent item ends inside an inline reference of type {kind}"
            ))
        })?;
        out.push(InlineRef {
            kind,
            payload: payload.to_vec(),
        });
        at += 1 + len;
    }
    Ok(out)
}

/// `body` with every inline shared reference naming `from` as its parent
/// naming `to` instead, and how many did.
///
/// The references are put back in the order the format keeps them in,
/// so a re-pointed one may change place among the shared references of
/// its type. The item keeps its size.
///
/// # Errors
///
/// As [`inline_refs`].
pub fn repoint_inline(item_type: u8, body: &[u8], from: u64, to: u64) -> Result<(Vec<u8>, usize)> {
    let start = refs_start(item_type, body)?;
    let mut refs = inline_refs(item_type, body)?;
    let mut moved = 0;
    for r in &mut refs {
        if matches!(r.kind, SHARED_BLOCK_REF | SHARED_DATA_REF) && r.offset() == from {
            r.payload[..8].copy_from_slice(&to.to_le_bytes());
            moved += 1;
        }
    }
    if moved == 0 {
        return Ok((body.to_vec(), 0));
    }
    // Stable, so keyed data references keep the order they were in.
    refs.sort_by_key(InlineRef::order);
    let mut out = body[..start].to_vec();
    for r in &refs {
        out.push(r.kind);
        out.extend_from_slice(&r.payload);
    }
    debug_assert_eq!(out.len(), body.len());
    Ok((out, moved))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An extent item header: refs, generation, flags.
    fn header(refs: u64, flags: u64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&refs.to_le_bytes());
        b.extend_from_slice(&7u64.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        b
    }

    fn shared_data(parent: u64, count: u32) -> Vec<u8> {
        let mut b = vec![SHARED_DATA_REF];
        b.extend_from_slice(&parent.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b
    }

    fn keyed_data(root: u64, ino: u64, offset: u64) -> Vec<u8> {
        let mut b = vec![EXTENT_DATA_REF];
        for v in [root, ino, offset] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&1u32.to_le_bytes());
        b
    }

    #[test]
    fn a_shared_data_reference_is_re_pointed_and_keeps_its_count() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(shared_data(4096, 2));
        let (out, moved) = repoint_inline(key_type::EXTENT_ITEM, &body, 4096, 8192).unwrap();
        assert_eq!(moved, 1);
        let mut expected = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        expected.extend(shared_data(8192, 2));
        assert_eq!(out, expected);
    }

    #[test]
    fn a_reference_to_another_parent_or_a_keyed_one_is_left_alone() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(keyed_data(5, 257, 0));
        body.extend(shared_data(12288, 1));
        let (out, moved) = repoint_inline(key_type::EXTENT_ITEM, &body, 4096, 8192).unwrap();
        assert_eq!((out, moved), (body, 0));
    }

    #[test]
    fn re_pointed_references_are_put_back_in_parent_order() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(keyed_data(5, 257, 0));
        body.extend(shared_data(4096, 1));
        body.extend(shared_data(12288, 1));
        let (out, moved) = repoint_inline(key_type::EXTENT_ITEM, &body, 4096, 16384).unwrap();
        assert_eq!(moved, 1);
        let mut expected = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        expected.extend(keyed_data(5, 257, 0));
        expected.extend(shared_data(12288, 1));
        expected.extend(shared_data(16384, 1));
        assert_eq!(out, expected);
    }

    #[test]
    fn a_shared_block_reference_in_a_skinny_item_is_re_pointed() {
        let flags = crate::extent_write::EXTENT_FLAG_TREE_BLOCK | BLOCK_FLAG_FULL_BACKREF;
        let mut body = header(1, flags);
        body.push(SHARED_BLOCK_REF);
        body.extend_from_slice(&4096u64.to_le_bytes());
        let (out, moved) = repoint_inline(key_type::METADATA_ITEM, &body, 4096, 8192).unwrap();
        assert_eq!(moved, 1);
        assert_eq!(out[ITEM_HEADER], SHARED_BLOCK_REF);
        assert_eq!(out[ITEM_HEADER + 1..], 8192u64.to_le_bytes());
    }

    #[test]
    fn a_tree_block_extent_item_skips_its_tree_block_info() {
        let mut body = header(1, crate::extent_write::EXTENT_FLAG_TREE_BLOCK);
        body.extend([0xAB; TREE_BLOCK_INFO]);
        body.push(SHARED_BLOCK_REF);
        body.extend_from_slice(&4096u64.to_le_bytes());
        let refs = inline_refs(key_type::EXTENT_ITEM, &body).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!((refs[0].kind, refs[0].offset()), (SHARED_BLOCK_REF, 4096));
    }

    #[test]
    fn an_unknown_reference_type_is_refused_rather_than_skipped() {
        let mut body = header(1, crate::extent_write::EXTENT_FLAG_DATA);
        body.push(99);
        body.extend([0; 8]);
        assert!(inline_refs(key_type::EXTENT_ITEM, &body).is_err());
    }

    #[test]
    fn a_body_ending_inside_a_reference_is_refused() {
        let mut body = header(1, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(&shared_data(4096, 1)[..6]);
        assert!(inline_refs(key_type::EXTENT_ITEM, &body).is_err());
    }
}
