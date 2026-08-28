//! Adding an item to a leaf, and taking one out.
//!
//! Recording an allocation means putting a `METADATA_ITEM` into the
//! extent tree, and releasing one means taking it out. Both are edits to
//! a leaf's item list, and both have to keep the list sorted — a B-tree
//! search bisects on that order, so an unsorted leaf is not slow, it
//! finds some items and silently misses others.
//!
//! [`crate::tree_write::build_leaf`] turns a list into bytes. This
//! produces the list.
//!
//! # Splitting is not implemented, and that is a measurement away
//!
//! When an item will not fit, the kernel splits the leaf in two and
//! pushes half the items into a new one. Where it puts the boundary is a
//! policy — roughly half, but "roughly" is doing work there, and a
//! writer that guesses produces leaves the kernel would not have
//! produced.
//!
//! Everything else in this crate's write path is byte-identical to what
//! the kernel writes because it was measured first. Guessing a split
//! point here would be the first place that stopped being true, so
//! instead an item that will not fit is refused, and the refusal names
//! what is missing.
//!
//! That was a judgement when it was written, and it is now a
//! measurement. Across 9,026 leaves of the two deep fixtures the median
//! is 91-98% FULL, with the dominant mode at 90-99% and only a
//! secondary cluster near half — see `docs/cow-transaction.md`. A split
//! down the middle would pile the distribution up at 50%, and it does
//! not. So "half" would be wrong in the common case, and wrong in a way
//! nothing here would catch: every other check in this crate compares
//! against blocks the kernel already wrote, not against blocks it would
//! write next.
//!
//! The fixture that would settle it is a leaf filled to just under
//! capacity and one more item added, captured either side.

use crate::chunk::DiskKey;
use crate::error::{Error, Result};
use crate::tree_write::{space_needed, LeafItem};

/// An item to place, owning its bytes.
///
/// [`LeafItem`] borrows, which is right for encoding a leaf that already
/// exists and wrong for building one that does not: the new item's bytes
/// have to outlive the list being assembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedItem {
    pub key: DiskKey,
    pub data: Vec<u8>,
}

impl OwnedItem {
    /// Borrow as a [`LeafItem`] for encoding.
    pub fn as_leaf_item(&self) -> LeafItem<'_> {
        LeafItem {
            key: self.key,
            data: &self.data,
        }
    }
}

/// Order two keys the way the tree is built on: objectid, then type,
/// then offset.
fn order(a: &DiskKey, b: &DiskKey) -> std::cmp::Ordering {
    (a.objectid, a.key_type, a.offset).cmp(&(b.objectid, b.key_type, b.offset))
}

/// Put `item` into `items`, keeping the order.
///
/// `nodesize` is the block size the result must fit in — the only thing
/// about the filesystem this needs, which is why it is a number rather
/// than a [`crate::superblock::Superblock`].
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] if an item with the same key is already
/// there — two items under one key is not a leaf a search can resolve,
/// and silently replacing one would lose whatever it held — or if the
/// result will not fit in a block, which is where splitting would go.
pub fn insert(nodesize: u32, items: &[OwnedItem], item: OwnedItem) -> Result<Vec<OwnedItem>> {
    let at = match items.binary_search_by(|existing| order(&existing.key, &item.key)) {
        Ok(_) => {
            return Err(Error::UnsupportedFeature(format!(
                "the leaf already holds an item under {:?}; replacing it is a different \
                 operation from inserting",
                item.key
            )))
        }
        Err(at) => at,
    };

    let mut out = Vec::with_capacity(items.len() + 1);
    out.extend_from_slice(&items[..at]);
    out.push(item);
    out.extend_from_slice(&items[at..]);

    let borrowed: Vec<LeafItem> = out.iter().map(|i| i.as_leaf_item()).collect();
    let needed = space_needed(&borrowed);
    let capacity = nodesize as usize;
    if needed > capacity {
        return Err(Error::UnsupportedFeature(format!(
            "the item does not fit: {} items need {needed} bytes and a leaf holds \
             {capacity}. Splitting a leaf is not implemented — where the kernel puts the \
             boundary is a policy this has not measured.",
            out.len()
        )));
    }
    Ok(out)
}

/// Take the item under `key` out.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] if no item has that key. Removing
/// something that is not there is not a no-op: for a caller releasing an
/// extent it means the extent tree does not say what the caller
/// believes, and continuing would leave a block recorded as allocated
/// for ever.
pub fn delete(items: &[OwnedItem], key: &DiskKey) -> Result<Vec<OwnedItem>> {
    let at = items
        .binary_search_by(|existing| order(&existing.key, key))
        .map_err(|_| {
            Error::UnsupportedFeature(format!(
                "the leaf holds no item under {key:?}, so there is nothing to remove and \
                 the caller's picture of the tree is wrong"
            ))
        })?;

    let mut out = Vec::with_capacity(items.len() - 1);
    out.extend_from_slice(&items[..at]);
    out.extend_from_slice(&items[at + 1..]);
    Ok(out)
}

/// Whether `item` would fit alongside `items`.
///
/// For a caller that would rather ask than handle the error — deciding
/// whether a transaction needs a split before it starts, rather than
/// halfway through.
pub fn fits(nodesize: u32, items: &[OwnedItem], item: &OwnedItem) -> bool {
    let mut borrowed: Vec<LeafItem> = items.iter().map(|i| i.as_leaf_item()).collect();
    borrowed.push(item.as_leaf_item());
    space_needed(&borrowed) <= nodesize as usize
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

    fn item(objectid: u64, len: usize) -> OwnedItem {
        OwnedItem {
            key: key(objectid, 1, 0),
            data: vec![objectid as u8; len],
        }
    }

    /// An item goes where the order puts it, not at the end.
    #[test]
    fn insertion_keeps_the_order_a_search_depends_on() {
        let items = vec![item(1, 8), item(5, 8), item(9, 8)];

        let out = insert(4096, &items, item(7, 8)).expect("inserting");
        let ids: Vec<u64> = out.iter().map(|i| i.key.objectid).collect();
        assert_eq!(ids, vec![1, 5, 7, 9]);

        // Before the first and after the last, which are the two an
        // insertion written as "find the gap" gets wrong.
        let out = insert(4096, &items, item(0, 8)).expect("inserting at the front");
        assert_eq!(out[0].key.objectid, 0);
        let out = insert(4096, &items, item(99, 8)).expect("inserting at the back");
        assert_eq!(out.last().unwrap().key.objectid, 99);
    }

    /// A duplicate key is refused rather than silently replacing.
    #[test]
    fn inserting_over_an_existing_key_is_refused() {
        let items = vec![item(1, 8), item(5, 8)];
        let err = insert(4096, &items, item(5, 16)).expect_err("5 is already there");
        assert!(err.to_string().contains("already holds"), "{err}");
    }

    /// An item that will not fit is refused, naming the missing piece.
    #[test]
    fn an_item_that_does_not_fit_is_refused_and_says_why() {
        let items = vec![item(1, 3000)];
        let err = insert(4096, &items, item(2, 3000)).expect_err("two 3000-byte items");
        assert!(
            err.to_string()
                .contains("Splitting a leaf is not implemented"),
            "the refusal should name what is missing: {err}"
        );
        assert!(!fits(4096, &items, &item(2, 3000)));
        assert!(fits(4096, &items, &item(2, 8)));
    }

    /// Removal takes out the one item and leaves the rest in order.
    #[test]
    fn deletion_removes_exactly_one_item() {
        let items = vec![item(1, 8), item(5, 8), item(9, 8)];
        let out = delete(&items, &key(5, 1, 0)).expect("deleting");
        let ids: Vec<u64> = out.iter().map(|i| i.key.objectid).collect();
        assert_eq!(ids, vec![1, 9]);
    }

    /// Removing something absent is an error, not a no-op.
    #[test]
    fn deleting_what_is_not_there_is_refused() {
        let items = vec![item(1, 8)];
        let err = delete(&items, &key(2, 1, 0)).expect_err("2 is not there");
        assert!(err.to_string().contains("no item under"), "{err}");
    }

    /// The whole key orders, not just the objectid.
    #[test]
    fn keys_that_share_an_objectid_order_by_type_then_offset() {
        let mut items = vec![
            OwnedItem {
                key: key(1, 1, 0),
                data: vec![0; 8],
            },
            OwnedItem {
                key: key(1, 1, 9),
                data: vec![0; 8],
            },
        ];
        items = insert(
            4096,
            &items,
            OwnedItem {
                key: key(1, 1, 4),
                data: vec![0; 8],
            },
        )
        .expect("inserting between two offsets of one objectid");
        let offsets: Vec<u64> = items.iter().map(|i| i.key.offset).collect();
        assert_eq!(offsets, vec![0, 4, 9]);
    }
}
