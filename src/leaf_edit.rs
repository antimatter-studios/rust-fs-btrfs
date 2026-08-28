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
//! # Splitting, and where the boundary goes
//!
//! When an item will not fit, the leaf splits in two. Where the kernel
//! puts the boundary is a policy, and it was measured rather than
//! guessed — `scripts/build-split-fixtures.sh` catches one split either
//! side of the event, and the two candidate rules were separated by a
//! second fixture built with deliberately uneven item sizes:
//!
//! ```text
//! items 12 to 232 bytes, 32 items across the split
//!   the kernel put              17 items / 1951 B on the left
//!   len/2 + 1 puts              17                  <- matches
//!   half the BYTES puts         14
//! ```
//!
//! And on the pair built with items of one size, 42 items became 22 and
//! 20 — again `len/2 + 1`. Note that this is one MORE than half, not
//! half rounded up: `div_ceil` gives 21 and 16 and is wrong on both.
//!
//! So the boundary is half the item count, not half the bytes. With
//! items of equal size the two rules agree and a measurement of such a
//! split says nothing, which is why the uneven fixture exists.
//!
//! A distribution had suggested otherwise and was wrong. Across 9,026
//! leaves of a populated filesystem the median is 91-98% FULL, which
//! looks like evidence against halving — but it is what halving
//! produces once the resulting leaves are filled up again by later
//! inserts. Reading a rule off a steady state was the mistake.
//!
//! # What is not claimed
//!
//! Both measured splits had an even item count, so `len/2 + 1` and
//! `(len + 1)/2 + ...` cannot be told apart for odd counts from this
//! evidence alone — the rule here is the one that fits, not the only
//! one that could.
//!
//! The kernel also biases towards where the new item is going, so a
//! split whose insertion point is far to one side may not match. The
//! fixtures do not cover that, and it is not claimed.

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

/// Split a list of items in two.
///
/// Returns the left and right halves. The boundary is half the item
/// count, rounding up so the left takes the odd one — see the module
/// docs for how that was measured, and for what about it is still a
/// choice.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] if there are fewer than two items:
/// there is no boundary that leaves something on both sides, and a
/// "split" producing an empty leaf is a leaf the tree has no use for.
///
/// It does NOT check that each half fits in a block. A split of a list
/// that was one item too big always does, and a caller assembling
/// something larger is doing something this does not model.
pub fn split(items: &[OwnedItem]) -> Result<(Vec<OwnedItem>, Vec<OwnedItem>)> {
    if items.len() < 2 {
        return Err(Error::UnsupportedFeature(format!(
            "a leaf of {} item(s) cannot be split into two that both hold something",
            items.len()
        )));
    }
    // Measured, not rounded to taste. Two captured splits, read from
    // the LIVE tree either side:
    //
    //     42 items -> 22 | 20        32 items -> 17 | 15
    //
    // Both are len/2 + 1, which is one MORE than half rather than half
    // rounded up. `div_ceil` would give 21 and 16 and be wrong on both.
    let mid = items.len() / 2 + 1;
    Ok((items[..mid].to_vec(), items[mid..].to_vec()))
}

/// Put `item` in, splitting if it will not fit.
///
/// Returns one list when it fitted and two when the leaf had to split.
/// The caller writes one block or two accordingly, and — when there are
/// two — must add the second to the parent, which is not done here
/// because this does not know what the parent is.
///
/// # Errors
///
/// As [`insert`], except that not fitting is no longer one.
pub fn insert_or_split(
    nodesize: u32,
    items: &[OwnedItem],
    item: OwnedItem,
) -> Result<Vec<Vec<OwnedItem>>> {
    if fits(nodesize, items, &item) {
        return Ok(vec![insert(nodesize, items, item)?]);
    }
    // Ordered first, then divided: the boundary is a position in the
    // final list, not in the one before the insert.
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
    let mut all = Vec::with_capacity(items.len() + 1);
    all.extend_from_slice(&items[..at]);
    all.push(item);
    all.extend_from_slice(&items[at..]);

    let (left, right) = split(&all)?;
    Ok(vec![left, right])
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
