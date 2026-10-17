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
//! keyed `(extent, type, parent)`. Inline ones are ordered by type and,
//! within a type, from the largest offset field down: the kernel's
//! `lookup_inline_extent_backref` scans them in that order, stops at the
//! first one smaller than what it wants, and inserts a new one there. So
//! an edit here takes references out and puts new ones in where that scan
//! would, and never re-sorts the rest. A keyed DATA reference is ordered
//! among its kind by a hash of its fields, which this driver does not
//! compute; a conversion that would have to place one among others is
//! refused by name rather than written in a guessed order.
//!
//! A snapshot shares tree blocks rather than copying them. When a tree
//! copies a block that another tree still reaches, the old block keeps
//! that other tree's reference and becomes `FULL_BACKREF`, and each of its
//! children gains a shared reference naming it -- the old block's own
//! pointers, which the snapshot still reads (#262): [`add_inline_shared`]
//! and [`drop_inline_tree_ref`].

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
}

/// Where the kernel puts a new inline reference of type `kind` and offset
/// field `offset` among `refs`: past every reference of a lower type, and
/// among its own type before the first whose offset is smaller. That is
/// where `lookup_inline_extent_backref`'s scan stops, and where it would
/// look for the reference again. An owner reference, which simple quotas
/// put first, is passed over as the scan passes over it. A keyed data
/// reference is ordered by a hash this does not compute, so the callers
/// place one only where it is the only one of its kind.
fn insert_position(refs: &[InlineRef], kind: u8, offset: u64) -> usize {
    refs.iter()
        .position(|r| {
            r.kind != EXTENT_OWNER_REF && (kind < r.kind || (kind == r.kind && r.offset() < offset))
        })
        .unwrap_or(refs.len())
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

/// Rebuild an item body from its fixed part and `refs`, in the order
/// given: each edit keeps the others where they were and puts a new one
/// where the kernel would ([`insert_position`]).
fn rebuild(body: &[u8], start: usize, refs: Vec<InlineRef>) -> Vec<u8> {
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
            let at = insert_position(&refs, TREE_BLOCK_REF, tree);
            refs.insert(
                at,
                InlineRef {
                    kind: TREE_BLOCK_REF,
                    payload: tree.to_le_bytes().to_vec(),
                },
            );
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
                // The only one of its kind, so its hash places it nowhere
                // in particular: after the lower types, before the higher.
                let at = insert_position(&refs, EXTENT_DATA_REF, 0);
                refs.insert(
                    at,
                    InlineRef {
                        kind: EXTENT_DATA_REF,
                        payload,
                    },
                );
            }
        }
    }
    Ok(rebuild(body, start, refs))
}

/// `body` with one reference of the inline `EXTENT_DATA_REF` naming
/// `tree`, `objectid` and `offset` dropped (#262): its count down by one,
/// or the reference removed when its count was one, and the item's total
/// down by one. `None` when the item holds no such reference inline.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] when the reference is the item's last:
/// dropping it is releasing the extent, which is not this edit. And as
/// [`inline_refs`].
pub fn drop_inline_data_ref(
    item_type: u8,
    body: &[u8],
    tree: u64,
    objectid: u64,
    offset: u64,
) -> Result<Option<Vec<u8>>> {
    let start = refs_start(item_type, body)?;
    let mut refs = inline_refs(item_type, body)?;
    let mut fields = Vec::with_capacity(24);
    for v in [tree, objectid, offset] {
        fields.extend_from_slice(&v.to_le_bytes());
    }
    let Some(at) = refs
        .iter()
        .position(|r| r.kind == EXTENT_DATA_REF && r.payload[..24] == fields[..])
    else {
        return Ok(None);
    };
    let total = u64::from_le_bytes(body[..8].try_into().expect("a fixed part"));
    if total <= 1 {
        return Err(Error::UnsupportedFeature(format!(
            "the data reference of inode {objectid} in tree {tree} at {offset} is its extent's \
             last, and dropping it is releasing the extent"
        )));
    }
    let count = u32::from_le_bytes(
        refs[at].payload[24..28]
            .try_into()
            .expect("28-byte payload"),
    );
    if count > 1 {
        refs[at].payload[24..28].copy_from_slice(&(count - 1).to_le_bytes());
    } else {
        refs.remove(at);
    }
    let mut head = body[..start].to_vec();
    head[..8].copy_from_slice(&(total - 1).to_le_bytes());
    Ok(Some(rebuild(&head, start, refs)))
}

/// `body` with one more reference from the file item of inode `objectid`
/// in `tree` that puts the extent's first byte at file offset `offset`
/// (#269): an inline `EXTENT_DATA_REF` already naming those three counted
/// up, or a new one put where the kernel puts it, and the item's total up
/// by one.
///
/// The kernel's place for a new one: past every inline reference of a
/// lower type, and among the data references before the first whose
/// [hash](crate::extent_write::data_ref_hash) is smaller than its own,
/// so they run from the largest hash down. It searches them in that order
/// and stops at the first smaller one, so a reference put anywhere else
/// is one it cannot find when it comes to drop it. Every other reference
/// keeps its place.
///
/// # Errors
///
/// As [`inline_refs`].
pub fn add_inline_data_ref(
    item_type: u8,
    body: &[u8],
    tree: u64,
    objectid: u64,
    offset: u64,
) -> Result<Vec<u8>> {
    use crate::extent_write::data_ref_hash;
    let start = refs_start(item_type, body)?;
    let mut refs = inline_refs(item_type, body)?;
    let mut fields = Vec::with_capacity(28);
    for v in [tree, objectid, offset] {
        fields.extend_from_slice(&v.to_le_bytes());
    }
    let hash_of = |p: &[u8]| {
        let field = |at: usize| u64::from_le_bytes(p[at..at + 8].try_into().expect("8 bytes"));
        data_ref_hash(field(0), field(8), field(16))
    };
    let want = data_ref_hash(tree, objectid, offset);

    let mut at = refs.len();
    let mut same = None;
    for (i, r) in refs.iter().enumerate() {
        if r.kind > EXTENT_DATA_REF {
            at = i;
            break;
        }
        if r.kind < EXTENT_DATA_REF {
            continue;
        }
        if r.payload[..24] == fields[..] {
            same = Some(i);
            break;
        }
        if hash_of(&r.payload) < want {
            at = i;
            break;
        }
    }
    match same {
        Some(i) => {
            let had = u32::from_le_bytes(refs[i].payload[24..28].try_into().expect("28 bytes"));
            let count = had.checked_add(1).ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "the data reference of inode {objectid} in tree {tree} at {offset} already \
                     counts {had} items, the most it can"
                ))
            })?;
            refs[i].payload[24..28].copy_from_slice(&count.to_le_bytes());
        }
        None => {
            fields.extend_from_slice(&1u32.to_le_bytes());
            refs.insert(
                at,
                InlineRef {
                    kind: EXTENT_DATA_REF,
                    payload: fields,
                },
            );
        }
    }
    let total = u64::from_le_bytes(body[..8].try_into().expect("a fixed part"));
    let total = total.checked_add(1).ok_or_else(|| {
        Error::UnsupportedFeature("the extent's reference count is at its limit".into())
    })?;
    let mut out = body[..start].to_vec();
    out[..8].copy_from_slice(&total.to_le_bytes());
    for r in &refs {
        out.push(r.kind);
        out.extend_from_slice(&r.payload);
    }
    Ok(out)
}

/// `body` with `count` shared references of type `kind` naming `parent`
/// added inline, and the item's total up by `count` (#262): what a child
/// of a block a snapshot shares gains when its tree copies that block. A
/// data extent the block's items point at more than once takes all of them
/// in one `SHARED_DATA_REF`, and one already naming `parent` takes the
/// count. The new reference goes where the kernel's scan would put it.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] for a tree block `parent` already names,
/// which would count one pointer twice, a count other than one for a tree
/// block, and a `kind` that is not a shared reference. And as
/// [`inline_refs`].
pub fn add_inline_shared(
    item_type: u8,
    body: &[u8],
    kind: u8,
    parent: u64,
    count: u32,
) -> Result<Vec<u8>> {
    let start = refs_start(item_type, body)?;
    let mut refs = inline_refs(item_type, body)?;
    let existing = refs
        .iter()
        .position(|r| r.kind == kind && r.offset() == parent);
    match (kind, existing) {
        (SHARED_DATA_REF, Some(at)) => {
            let had = shared_count(&refs[at]);
            refs[at].payload[8..12].copy_from_slice(&(had + count).to_le_bytes());
        }
        (SHARED_DATA_REF, None) => {
            let mut payload = parent.to_le_bytes().to_vec();
            payload.extend_from_slice(&count.to_le_bytes());
            let at = insert_position(&refs, kind, parent);
            refs.insert(at, InlineRef { kind, payload });
        }
        (SHARED_BLOCK_REF, Some(_)) => {
            return Err(Error::UnsupportedFeature(format!(
                "the tree block is already referred to by the block at {parent}, so another \
                 reference from it would count one pointer twice"
            )))
        }
        (SHARED_BLOCK_REF, None) if count == 1 => {
            let at = insert_position(&refs, kind, parent);
            refs.insert(
                at,
                InlineRef {
                    kind,
                    payload: parent.to_le_bytes().to_vec(),
                },
            );
        }
        _ => {
            return Err(Error::UnsupportedFeature(format!(
                "{count} shared reference(s) of type {kind} from the block at {parent} is not \
                 a reference copy-on-write adds"
            )))
        }
    }
    let total = u64::from_le_bytes(body[..8].try_into().expect("a fixed part"));
    let mut head = body[..start].to_vec();
    head[..8].copy_from_slice(&(total + u64::from(count)).to_le_bytes());
    Ok(rebuild(&head, start, refs))
}

/// `body` without its inline `TREE_BLOCK_REF` naming `tree`, and the
/// item's total down by one (#262): a tree block that tree has copied, and
/// that another tree still points at. `None` when the item holds no such
/// reference inline.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] when the reference is the item's last:
/// dropping it is releasing the block, which is not this edit. And as
/// [`inline_refs`].
pub fn drop_inline_tree_ref(item_type: u8, body: &[u8], tree: u64) -> Result<Option<Vec<u8>>> {
    let start = refs_start(item_type, body)?;
    let mut refs = inline_refs(item_type, body)?;
    let Some(at) = refs
        .iter()
        .position(|r| r.kind == TREE_BLOCK_REF && r.offset() == tree)
    else {
        return Ok(None);
    };
    let total = u64::from_le_bytes(body[..8].try_into().expect("a fixed part"));
    if total <= 1 {
        return Err(Error::UnsupportedFeature(format!(
            "the reference from tree {tree} is its tree block's last, and dropping it is \
             releasing the block"
        )));
    }
    refs.remove(at);
    let mut head = body[..start].to_vec();
    head[..8].copy_from_slice(&(total - 1).to_le_bytes());
    Ok(Some(rebuild(&head, start, refs)))
}

/// `body` with `flags` set besides those it has.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] for a body too short to have flags.
pub fn with_flags(body: &[u8], set: u64) -> Result<Vec<u8>> {
    let had = flags(body).ok_or_else(|| short(body.len()))?;
    let mut out = body.to_vec();
    out[FLAGS..FLAGS + 8].copy_from_slice(&(had | set).to_le_bytes());
    Ok(out)
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

    /// An inline data reference: tree, inode, offset, count.
    fn data_ref(tree: u64, ino: u64, offset: u64, count: u32) -> Vec<u8> {
        let mut b = vec![EXTENT_DATA_REF];
        for v in [tree, ino, offset] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&count.to_le_bytes());
        b
    }

    #[test]
    fn dropping_one_of_two_data_references_keeps_the_other() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(data_ref(5, 257, 0, 1));
        body.extend(data_ref(5, 258, 0, 1));
        let dropped = drop_inline_data_ref(key_type::EXTENT_ITEM, &body, 5, 257, 0)
            .unwrap()
            .expect("the reference is there");
        let mut want = header(1, crate::extent_write::EXTENT_FLAG_DATA);
        want.extend(data_ref(5, 258, 0, 1));
        assert_eq!(dropped, want);
    }

    #[test]
    fn dropping_one_of_a_counted_data_reference_counts_it_down() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(data_ref(5, 257, 0, 2));
        let dropped = drop_inline_data_ref(key_type::EXTENT_ITEM, &body, 5, 257, 0)
            .unwrap()
            .expect("the reference is there");
        let mut want = header(1, crate::extent_write::EXTENT_FLAG_DATA);
        want.extend(data_ref(5, 257, 0, 1));
        assert_eq!(dropped, want);
    }

    #[test]
    fn a_data_reference_held_elsewhere_or_the_last_is_not_dropped() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(data_ref(5, 258, 0, 1));
        body.extend(shared_data(4096, 1));
        assert_eq!(
            drop_inline_data_ref(key_type::EXTENT_ITEM, &body, 5, 257, 0).unwrap(),
            None
        );
        let mut last = header(1, crate::extent_write::EXTENT_FLAG_DATA);
        last.extend(data_ref(5, 257, 0, 1));
        assert!(drop_inline_data_ref(key_type::EXTENT_ITEM, &last, 5, 257, 0).is_err());
    }

    /// The hash of an inline data reference built by [`data_ref`].
    fn hash(tree: u64, ino: u64, offset: u64) -> u64 {
        crate::extent_write::data_ref_hash(tree, ino, offset)
    }

    #[test]
    fn an_added_data_reference_counts_up_one_already_there() {
        let mut body = header(1, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(data_ref(5, 257, 0, 1));
        let added = add_inline_data_ref(key_type::EXTENT_ITEM, &body, 5, 257, 0).unwrap();
        let mut want = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        want.extend(data_ref(5, 257, 0, 2));
        assert_eq!(added, want);
    }

    #[test]
    fn added_data_references_run_from_the_largest_hash_down() {
        // Six references added one at a time, in whatever order: the item
        // holds them by descending hash, whichever order they came in.
        let wanted: Vec<(u64, u64, u64)> = (257..263).map(|ino| (5, ino, 0)).collect();
        let mut body = header(0, crate::extent_write::EXTENT_FLAG_DATA);
        for &(t, i, o) in wanted.iter().rev() {
            body = add_inline_data_ref(key_type::EXTENT_ITEM, &body, t, i, o).unwrap();
        }
        let mut sorted = wanted.clone();
        sorted.sort_by_key(|&(t, i, o)| std::cmp::Reverse(hash(t, i, o)));
        let mut want = header(6, crate::extent_write::EXTENT_FLAG_DATA);
        for (t, i, o) in sorted {
            want.extend(data_ref(t, i, o, 1));
        }
        assert_eq!(body, want);

        let mut again = header(0, crate::extent_write::EXTENT_FLAG_DATA);
        for &(t, i, o) in &wanted {
            again = add_inline_data_ref(key_type::EXTENT_ITEM, &again, t, i, o).unwrap();
        }
        assert_eq!(
            again, want,
            "the order depends on the hashes, not on arrival"
        );
    }

    #[test]
    fn an_added_data_reference_goes_after_lower_types_and_before_shared_ones() {
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.push(EXTENT_OWNER_REF);
        body.extend_from_slice(&5u64.to_le_bytes());
        body.extend(shared_data(4096, 1));
        let added = add_inline_data_ref(key_type::EXTENT_ITEM, &body, 5, 300, 8192).unwrap();
        let mut want = header(3, crate::extent_write::EXTENT_FLAG_DATA);
        want.push(EXTENT_OWNER_REF);
        want.extend_from_slice(&5u64.to_le_bytes());
        want.extend(data_ref(5, 300, 8192, 1));
        want.extend(shared_data(4096, 1));
        assert_eq!(added, want);
    }

    #[test]
    fn adding_a_data_reference_leaves_the_others_where_they_were() {
        // Two shared references in an order this module's own sort would
        // reverse: adding a data reference must not reorder them.
        let mut body = header(2, crate::extent_write::EXTENT_FLAG_DATA);
        body.extend(shared_data(12288, 1));
        body.extend(shared_data(4096, 1));
        let added = add_inline_data_ref(key_type::EXTENT_ITEM, &body, 5, 257, 0).unwrap();
        let mut want = header(3, crate::extent_write::EXTENT_FLAG_DATA);
        want.extend(data_ref(5, 257, 0, 1));
        want.extend(shared_data(12288, 1));
        want.extend(shared_data(4096, 1));
        assert_eq!(added, want);
    }

    /// A tree block reference: the tree's objectid.
    fn tree_ref(tree: u64) -> Vec<u8> {
        let mut b = vec![TREE_BLOCK_REF];
        b.extend_from_slice(&tree.to_le_bytes());
        b
    }

    fn shared_block(parent: u64) -> Vec<u8> {
        let mut b = vec![SHARED_BLOCK_REF];
        b.extend_from_slice(&parent.to_le_bytes());
        b
    }

    const BLOCK: u64 = crate::extent_write::EXTENT_FLAG_TREE_BLOCK;
    const DATA: u64 = crate::extent_write::EXTENT_FLAG_DATA;

    #[test]
    fn a_copied_shared_block_keeps_the_other_trees_reference_alone() {
        // As the kernel leaves a leaf a snapshot of tree 5 shares: the
        // larger root first.
        let mut body = header(2, BLOCK);
        body.extend(tree_ref(256));
        body.extend(tree_ref(5));
        let dropped = drop_inline_tree_ref(key_type::METADATA_ITEM, &body, 5)
            .unwrap()
            .expect("tree 5's reference is there");
        let mut want = header(1, BLOCK);
        want.extend(tree_ref(256));
        assert_eq!(dropped, want);
        // Put back, it goes where the kernel's scan finds it again.
        let back = add_inline_keyed(
            key_type::METADATA_ITEM,
            &header(1, BLOCK)
                .into_iter()
                .chain(tree_ref(256))
                .collect::<Vec<_>>(),
            Keyed::Block { tree: 5 },
            1,
        )
        .unwrap();
        assert_eq!(back[ITEM_HEADER..], body[ITEM_HEADER..]);
        let flagged = with_flags(&dropped, BLOCK_FLAG_FULL_BACKREF).unwrap();
        assert_eq!(flags(&flagged), Some(BLOCK | BLOCK_FLAG_FULL_BACKREF));
        assert_eq!(flagged[ITEM_HEADER..], dropped[ITEM_HEADER..]);
    }

    #[test]
    fn a_tree_reference_absent_or_last_is_not_dropped() {
        let mut body = header(2, BLOCK);
        body.extend(tree_ref(256));
        body.extend(shared_block(4096));
        assert_eq!(
            drop_inline_tree_ref(key_type::METADATA_ITEM, &body, 5).unwrap(),
            None
        );
        let mut last = header(1, BLOCK);
        last.extend(tree_ref(5));
        assert!(drop_inline_tree_ref(key_type::METADATA_ITEM, &last, 5).is_err());
    }

    #[test]
    fn a_child_of_a_copied_shared_block_gains_a_reference_naming_it() {
        let mut body = header(1, BLOCK);
        body.extend(tree_ref(5));
        let out =
            add_inline_shared(key_type::METADATA_ITEM, &body, SHARED_BLOCK_REF, 8192, 1).unwrap();
        let mut want = header(2, BLOCK);
        want.extend(tree_ref(5));
        want.extend(shared_block(8192));
        assert_eq!(out, want);
        // Twice from one parent is one pointer counted twice.
        assert!(
            add_inline_shared(key_type::METADATA_ITEM, &out, SHARED_BLOCK_REF, 8192, 1).is_err()
        );
    }

    #[test]
    fn shared_references_of_one_type_run_from_the_largest_parent_down() {
        let mut body = header(2, DATA);
        body.extend(keyed_data(5, 257, 0));
        body.extend(shared_data(4096, 1));
        let out =
            add_inline_shared(key_type::EXTENT_ITEM, &body, SHARED_DATA_REF, 12288, 1).unwrap();
        let mut want = header(3, DATA);
        want.extend(keyed_data(5, 257, 0));
        want.extend(shared_data(12288, 1));
        want.extend(shared_data(4096, 1));
        assert_eq!(out, want);
        let smaller =
            add_inline_shared(key_type::EXTENT_ITEM, &body, SHARED_DATA_REF, 1024, 2).unwrap();
        let mut want = header(4, DATA);
        want.extend(keyed_data(5, 257, 0));
        want.extend(shared_data(4096, 1));
        want.extend(shared_data(1024, 2));
        assert_eq!(smaller, want);
    }

    #[test]
    fn a_shared_data_reference_from_the_same_parent_takes_the_count() {
        let mut body = header(2, DATA);
        body.extend(keyed_data(5, 257, 0));
        body.extend(shared_data(4096, 1));
        let out =
            add_inline_shared(key_type::EXTENT_ITEM, &body, SHARED_DATA_REF, 4096, 2).unwrap();
        let mut want = header(4, DATA);
        want.extend(keyed_data(5, 257, 0));
        want.extend(shared_data(4096, 3));
        assert_eq!(out, want);
    }

    #[test]
    fn removing_a_reference_leaves_the_others_in_the_order_found() {
        let mut body = header(3, DATA);
        body.extend(keyed_data(5, 257, 0));
        body.extend(shared_data(12288, 1));
        body.extend(shared_data(4096, 1));
        let dropped = drop_inline_data_ref(key_type::EXTENT_ITEM, &body, 5, 257, 0)
            .unwrap()
            .unwrap();
        let mut want = header(2, DATA);
        want.extend(shared_data(12288, 1));
        want.extend(shared_data(4096, 1));
        assert_eq!(dropped, want);
    }
}
