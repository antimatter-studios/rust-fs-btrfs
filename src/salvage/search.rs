//! Items in a key range, found by descending a tree past its damage.
//!
//! [`super::walk_tree`] reads a whole tree. Finding one inode's extents or
//! names needs only the leaves that hold its keys: descend to the first
//! key, take the items in range, and descend again from the next key the
//! nodes above promised, until the range ends. A damaged block on the way
//! is recorded and its subtree skipped, so the rest of the range is still
//! found.

use super::tree::{
    read_block, BlockFault, DamagedBlock, Expect, Layout, RootPointer, SalvagedBlock, SalvagedBody,
};
use crate::btree::{compare_keys, Item};
use crate::chunk::DiskKey;
use fs_core::BlockRead;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::ops::ControlFlow;
use std::rc::Rc;

/// Descents one search makes at most, so keys out of order cannot loop it.
const MAX_DESCENTS: usize = 1 << 20;

/// What [`search_range`] met on the way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchReport {
    /// Every damaged block the descents read, once each.
    pub damaged: Vec<DamagedBlock>,
    /// False when a damaged block hid part of the range: its subtree was
    /// skipped, and items in it may be missing.
    pub complete: bool,
    /// Set when the visitor stopped the search.
    pub stopped: bool,
}

fn record(
    report: &mut SearchReport,
    seen: &mut HashSet<u64>,
    block: &SalvagedBlock,
    expect: Expect,
) {
    if (!block.is_verified() || block.attempts.len() > 1) && seen.insert(block.logical) {
        report.damaged.push(DamagedBlock {
            logical: block.logical,
            expected_generation: expect.generation,
            faults: block.faults.clone(),
            attempts: block.attempts.clone(),
            decoded: !matches!(block.body, SalvagedBody::None),
        });
    }
}

/// Hand every item whose key lies in `[start, end]` to `visit`, in key
/// order, with the leaf it is in. Items from damaged leaves are handed over
/// too; [`SalvagedBlock::is_verified`] says which.
pub fn search_range(
    source: &dyn BlockRead,
    layout: &Layout,
    root: RootPointer,
    start: DiskKey,
    end: DiskKey,
    visit: &mut dyn FnMut(&SalvagedBlock, &Item) -> ControlFlow<()>,
) -> SearchReport {
    let mut report = SearchReport {
        complete: true,
        ..SearchReport::default()
    };
    let mut seen = HashSet::new();
    let mut from = start;
    for _ in 0..MAX_DESCENTS {
        let mut expect = Expect {
            generation: root.generation,
            level: root.level,
        };
        let mut block = read_block(source, layout, root.bytenr, expect);
        let mut bound: Option<DiskKey> = None;
        loop {
            record(&mut report, &mut seen, &block, expect);
            match &block.body {
                SalvagedBody::None => {
                    report.complete = false;
                    break;
                }
                SalvagedBody::Node(pointers) => {
                    let slot = pointers
                        .iter()
                        .rposition(|p| compare_keys(&p.key, &from) != Ordering::Greater)
                        .unwrap_or(0);
                    if let Some(next) = pointers.get(slot + 1) {
                        if bound.is_none_or(|b| compare_keys(&next.key, &b) == Ordering::Less) {
                            bound = Some(next.key);
                        }
                    }
                    let Some(pointer) = pointers.get(slot) else {
                        report.complete = false;
                        break;
                    };
                    let level = block.header.as_ref().map_or(1, |h| h.level);
                    expect = Expect {
                        generation: Some(pointer.generation),
                        level: level.checked_sub(1),
                    };
                    block = read_block(source, layout, pointer.blockptr, expect);
                }
                SalvagedBody::Leaf(items) => {
                    for item in items {
                        if compare_keys(&item.key, &from) == Ordering::Less {
                            continue;
                        }
                        if compare_keys(&item.key, &end) == Ordering::Greater {
                            return report;
                        }
                        if visit(&block, item).is_break() {
                            report.stopped = true;
                            return report;
                        }
                    }
                    break;
                }
            }
        }
        match bound {
            Some(next)
                if compare_keys(&next, &from) == Ordering::Greater
                    && compare_keys(&next, &end) != Ordering::Greater =>
            {
                from = next
            }
            _ => return report,
        }
    }
    report.complete = false;
    report
}

/// The items of one object and key type, `(objectid, key_type, *)`, with
/// the leaf each is in. Items of one leaf share it.
pub fn object_items(
    source: &dyn BlockRead,
    layout: &Layout,
    root: RootPointer,
    objectid: u64,
    key_type: u8,
) -> (Vec<(Rc<SalvagedBlock>, Item)>, SearchReport) {
    let mut found: Vec<(Rc<SalvagedBlock>, Item)> = Vec::new();
    let report = search_range(
        source,
        layout,
        root,
        DiskKey {
            objectid,
            key_type,
            offset: 0,
        },
        DiskKey {
            objectid,
            key_type,
            offset: u64::MAX,
        },
        &mut |leaf, item| {
            let block = match found.last() {
                Some((last, _)) if last.logical == leaf.logical => Rc::clone(last),
                _ => Rc::new(leaf.clone()),
            };
            found.push((block, *item));
            ControlFlow::Continue(())
        },
    );
    (found, report)
}

/// Whether a block fault means the bytes could not be had at all.
pub fn is_unreadable(fault: &BlockFault) -> bool {
    matches!(
        fault,
        BlockFault::Unreadable(_) | BlockFault::Unmapped | BlockFault::OtherDevice { .. }
    )
}
