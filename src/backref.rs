//! The back references an extent record carries, and what a move does to
//! the ones naming the moved block (#287).
//!
//! An extent item says who refers to the extent. Each reference is either
//! KEYED — "tree 5 refers to this", or for data "inode 588 of tree 5, at
//! file offset 0" — or SHARED, naming the address of the tree block that
//! holds the pointer: its parent. Which one a block's children get is the
//! block's own choice, recorded in its extent item as `FULL_BACKREF`.
//! Balance leaves blocks flagged that way, because it relocates through a
//! tree that is not the one the blocks finally belong to.
//!
//! A copy-on-write move writes a NEW block, owned by the tree it sits in,
//! in a generation no snapshot has seen. Nothing can share that block, so
//! `btrfs check` expects it to be an ordinary one: no `FULL_BACKREF` flag,
//! and its children referred to by the tree rather than by its address.
//! Keeping the flag and re-pointing the shared references at the new
//! address was tried first, and `btrfs check` called it a "bad full
//! backref" and wanted keyed references for every extent under the leaf.
//! So the move converts each shared reference naming the old block into
//! the keyed one the new block's children carry, with the same count:
//! [`take_inline_shared`] takes the shared one out, and
//! [`add_inline_keyed`] puts the keyed one in.
//!
//! References live inline in the extent item when there is room, as a
//! type byte followed by a payload, and otherwise as items of their own
//! keyed `(extent, type, parent)`. Inline ones are ordered by type and
//! then by their offset field. A keyed DATA reference is ordered among
//! its kind by a hash of its fields, which this driver does not compute;
//! a conversion that would have to place one among others is refused by
//! name rather than written in a guessed order.

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
    /// is ordered by a hash of its fields, which this code does not
    /// compute: [`add_inline_keyed`] only adds one where it is the only
    /// one of its kind, so those already there keep the order they were
    /// found in.
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

/// The keyed reference a shared one becomes when the block it names is
/// replaced by an ordinary block of its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Keyed {
    /// A child tree block: `TREE_BLOCK_REF` naming the tree.
    Block {
        /// The tree the parent belongs to.
        tree: u64,
    },
    /// A data extent: `EXTENT_DATA_REF` naming the tree, the inode and
    /// the file offset of the extent's first byte.
    Data {
        /// The tree the leaf belongs to.
        tree: u64,
        /// The inode whose items point at the extent.
        objectid: u64,
        /// The file offset the extent's first byte would sit at: the
        /// item's key offset less its offset into the extent.
        offset: u64,
    },
}

impl Keyed {
    /// The shared reference type this replaces.
    pub fn shared_kind(&self) -> u8 {
        match self {
            Keyed::Block { .. } => SHARED_BLOCK_REF,
            Keyed::Data { .. } => SHARED_DATA_REF,
        }
    }
}

/// The count a shared reference carries: one for a tree block, the
/// trailing `u32` for data.
fn shared_count(r: &InlineRef) -> u32 {
    if r.kind == SHARED_DATA_REF {
        u32::from_le_bytes(r.payload[8..12].try_into().expect("12-byte payload"))
    } else {
        1
    }
}

/// Rebuild an item body from its fixed part and `refs`, in the order the
/// format keeps them.
fn rebuild(body: &[u8], start: usize, mut refs: Vec<InlineRef>) -> Vec<u8> {
    // Stable, so keyed data references keep the order they were in.
    refs.sort_by_key(InlineRef::order);
    let mut out = body[..start].to_vec();
    for r in &refs {
        out.push(r.kind);
        out.extend_from_slice(&r.payload);
    }
    out
}

/// `body` without its inline shared reference of type `kind` naming
/// `parent`, and that reference's count; `None` when it holds no such
/// reference inline.
///
/// # Errors
///
/// As [`inline_refs`].
pub fn take_inline_shared(
    item_type: u8,
    body: &[u8],
    kind: u8,
    parent: u64,
) -> Result<Option<(Vec<u8>, u32)>> {
    let start = refs_start(item_type, body)?;
    let mut refs = inline_refs(item_type, body)?;
    let Some(at) = refs
        .iter()
        .position(|r| r.kind == kind && r.offset() == parent)
    else {
        return Ok(None);
    };
    let taken = refs.remove(at);
    Ok(Some((rebuild(body, start, refs), shared_count(&taken))))
}

/// `body` with the keyed reference `keyed` added inline, carrying
/// `count`, merged into an identical one when the item already holds it.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] for a tree block the tree already
/// refers to, which would count one reference twice, and for a data
/// reference that would sit beside another keyed data reference: those
/// are ordered by a hash of their fields that this driver does not
/// compute, and a guessed order is one the kernel searches past. And as
/// [`inline_refs`].
pub fn add_inline_keyed(item_type: u8, body: &[u8], keyed: Keyed, count: u32) -> Result<Vec<u8>> {
    let start = refs_start(item_type, body)?;
    let mut refs = inline_refs(item_type, body)?;
    match keyed {
        Keyed::Block { tree } => {
            if refs
                .iter()
                .any(|r| r.kind == TREE_BLOCK_REF && r.offset() == tree)
            {
                return Err(Error::UnsupportedFeature(format!(
                    "the tree block already carries a reference from tree {tree}, so \
                     converting its shared reference would count that tree twice"
                )));
            }
            refs.push(InlineRef {
                kind: TREE_BLOCK_REF,
                payload: tree.to_le_bytes().to_vec(),
            });
        }
        Keyed::Data {
            tree,
            objectid,
            offset,
        } => {
            let mut fields = Vec::with_capacity(24);
            for v in [tree, objectid, offset] {
                fields.extend_from_slice(&v.to_le_bytes());
            }
            let data_refs: Vec<usize> = refs
                .iter()
                .enumerate()
                .filter(|(_, r)| r.kind == EXTENT_DATA_REF)
                .map(|(i, _)| i)
                .collect();
            if let Some(&same) = data_refs
                .iter()
                .find(|&&i| refs[i].payload[..24] == fields[..])
            {
                let had = u32::from_le_bytes(
                    refs[same].payload[24..28]
                        .try_into()
                        .expect("28-byte payload"),
                );
                refs[same].payload[24..28].copy_from_slice(&(had + count).to_le_bytes());
            } else if !data_refs.is_empty() {
                return Err(Error::UnsupportedFeature(format!(
                    "the data extent already carries {} other keyed data reference(s), and \
                     a new one is ordered among them by a hash of its fields that this \
                     driver does not compute",
                    data_refs.len()
                )));
            } else {
                let mut payload = fields;
                payload.extend_from_slice(&count.to_le_bytes());
                refs.push(InlineRef {
                    kind: EXTENT_DATA_REF,
                    payload,
                });
            }
        }
    }
    Ok(rebuild(body, start, refs))
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

    fn keyed_count(root: u64, ino: u64, offset: u64, count: u32) -> Vec<u8> {
        let mut b = keyed_data(root, ino, offset);
        let n = b.len();
        b[n - 4..].copy_from_slice(&count.to_le_bytes());
        b
    }

    fn data(tree: u64, objectid: u64, offset: u64) -> Keyed {
        Keyed::Data {
            tree,
            objectid,
            offset,
        }
    }

    #[test]
    fn a_shared_data_reference_becomes_a_keyed_one_and_keeps_its_count() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(shared_data(4096, 2));
        let (rest, count) = take_inline_shared(key_type::EXTENT_ITEM, &body, SHARED_DATA_REF, 4096)
            .unwrap()
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(rest, header(2, crate::extent_write::EXTENT_FLAG_DATA));
        let out = add_inline_keyed(key_type::EXTENT_ITEM, &rest, data(5, 588, 0), count).unwrap();
        let mut expected = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        expected.extend(keyed_count(5, 588, 0, 2));
        assert_eq!(out, expected);
    }

    #[test]
    fn a_reference_to_another_parent_or_a_keyed_one_is_left_alone() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(keyed_data(5, 257, 0));
        body.extend(shared_data(12288, 1));
        assert_eq!(
            take_inline_shared(key_type::EXTENT_ITEM, &body, SHARED_DATA_REF, 4096).unwrap(),
            None
        );
        // A block reference naming the same address is not a data one.
        assert_eq!(
            take_inline_shared(key_type::EXTENT_ITEM, &body, SHARED_BLOCK_REF, 12288).unwrap(),
            None
        );
    }

    #[test]
    fn a_converted_reference_goes_before_the_shared_ones_it_left() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(shared_data(4096, 1));
        body.extend(shared_data(12288, 1));
        let (rest, count) =
            take_inline_shared(key_type::EXTENT_ITEM, &body, SHARED_DATA_REF, 12288)
                .unwrap()
                .unwrap();
        let out = add_inline_keyed(key_type::EXTENT_ITEM, &rest, data(5, 257, 0), count).unwrap();
        let mut expected = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        expected.extend(keyed_data(5, 257, 0));
        expected.extend(shared_data(4096, 1));
        assert_eq!(out, expected);
    }

    #[test]
    fn an_identical_keyed_data_reference_takes_the_count() {
        let mut body = header(3, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(keyed_data(5, 257, 0));
        body.extend(shared_data(4096, 2));
        let (rest, count) = take_inline_shared(key_type::EXTENT_ITEM, &body, SHARED_DATA_REF, 4096)
            .unwrap()
            .unwrap();
        let out = add_inline_keyed(key_type::EXTENT_ITEM, &rest, data(5, 257, 0), count).unwrap();
        let mut expected = header(3, crate::extent_write::EXTENT_FLAG_DATA);
        expected.extend(keyed_count(5, 257, 0, 3));
        assert_eq!(out, expected);
    }

    #[test]
    fn a_keyed_data_reference_beside_a_different_one_is_refused() {
        let mut body = header(1, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(keyed_data(5, 257, 0));
        let err = add_inline_keyed(key_type::EXTENT_ITEM, &body, data(5, 258, 0), 1).unwrap_err();
        assert!(err.to_string().contains("hash"), "{err}");
    }

    #[test]
    fn a_shared_block_reference_in_a_skinny_item_becomes_a_tree_block_reference() {
        let block_flags = crate::extent_write::EXTENT_FLAG_TREE_BLOCK | BLOCK_FLAG_FULL_BACKREF;
        let mut body = header(1, block_flags);
        body.push(SHARED_BLOCK_REF);
        body.extend_from_slice(&4096u64.to_le_bytes());
        let (rest, count) =
            take_inline_shared(key_type::METADATA_ITEM, &body, SHARED_BLOCK_REF, 4096)
                .unwrap()
                .unwrap();
        assert_eq!(count, 1);
        let out = add_inline_keyed(
            key_type::METADATA_ITEM,
            &rest,
            Keyed::Block { tree: 5 },
            count,
        )
        .unwrap();
        assert_eq!(out.len(), body.len());
        assert_eq!(out[ITEM_HEADER], TREE_BLOCK_REF);
        assert_eq!(out[ITEM_HEADER + 1..], 5u64.to_le_bytes());
        // The block's own flag is not this function's to change.
        assert_eq!(flags(&out), Some(block_flags));
    }

    #[test]
    fn a_tree_already_referring_to_the_block_is_refused() {
        let mut body = header(1, crate::extent_write::EXTENT_FLAG_TREE_BLOCK);
        body.push(TREE_BLOCK_REF);
        body.extend_from_slice(&5u64.to_le_bytes());
        assert!(
            add_inline_keyed(key_type::METADATA_ITEM, &body, Keyed::Block { tree: 5 }, 1).is_err()
        );
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
