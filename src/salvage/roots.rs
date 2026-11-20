//! Tree roots: the ones the root tree records, and the older root trees
//! a restore-style search starts from.
//!
//! Btrfs never overwrites a tree in place. Every commit writes new blocks
//! and leaves the previous generation's on disk until their space is
//! reused, and each superblock copy names four earlier root trees in its
//! backup ring. When the newest root tree is damaged, an older one often
//! still leads to most of the volume, as it stood a few commits before.

use super::superblock::{CopyVerdict, SuperblockCopy};
use super::tree::{walk_tree, Layout, RootPointer, WalkReport};
use crate::chunk::DiskKey;
use crate::fs::{root_item, ROOT_ITEM_KEY};
use crate::superblock::{le64, Superblock};
use fs_core::BlockRead;
use std::ops::ControlFlow;

/// One `ROOT_ITEM`: where a tree's root block is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootItem {
    /// The tree's objectid: 2 for the extent tree, 5 for the top-level
    /// filesystem tree, 256 and up for subvolumes and snapshots.
    pub objectid: u64,
    /// The key's offset: 0, or for a snapshot the generation it was taken
    /// at.
    pub key_offset: u64,
    pub bytenr: u64,
    pub generation: u64,
    pub level: u8,
    /// The inode number of the tree's top directory.
    pub root_dirid: u64,
    pub flags: u64,
    /// `generation_v2`, when the item is long enough to hold it.
    pub generation_v2: Option<u64>,
}

impl RootItem {
    /// Decode a `ROOT_ITEM`, or `None` when the key is another type or the
    /// item is too short for the fields a walk needs.
    pub fn parse(key: &DiskKey, data: &[u8]) -> Option<Self> {
        if key.key_type != ROOT_ITEM_KEY || data.len() <= root_item::LEVEL {
            return None;
        }
        Some(Self {
            objectid: key.objectid,
            key_offset: key.offset,
            bytenr: le64(data, root_item::BYTENR),
            generation: le64(data, root_item::GENERATION),
            level: data[root_item::LEVEL],
            root_dirid: le64(data, root_item::ROOT_DIRID),
            flags: le64(data, root_item::FLAGS),
            generation_v2: (data.len() >= root_item::GENERATION_V2 + 8)
                .then(|| le64(data, root_item::GENERATION_V2)),
        })
    }

    /// Where the tree starts, with the generation and level the item
    /// records for it.
    pub fn pointer(&self) -> RootPointer {
        RootPointer {
            bytenr: self.bytenr,
            generation: Some(self.generation),
            level: Some(self.level),
        }
    }
}

/// Every `ROOT_ITEM` a root tree still yields, and the walk that found
/// them. Items from damaged leaves are included; the walk names the leaves.
pub fn read_root_items(
    source: &dyn BlockRead,
    layout: &Layout,
    tree_root: RootPointer,
) -> (Vec<RootItem>, WalkReport) {
    let mut roots = Vec::new();
    let walk = walk_tree(source, layout, tree_root, &mut |leaf| {
        for item in leaf.items() {
            if let Some(root) = leaf
                .item_data(item)
                .and_then(|data| RootItem::parse(&item.key, data))
            {
                roots.push(root);
            }
        }
        ControlFlow::Continue(())
    });
    (roots, walk)
}

/// Where a root tree came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// The `root` field of a superblock copy.
    Superblock { mirror: usize },
    /// A backup slot of a superblock copy.
    Backup { mirror: usize, slot: usize },
}

/// A root tree a search can start from, with everything known about it
/// before it is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootTreeCandidate {
    /// Every place that names this root tree. The same root appears in
    /// several copies, and in a backup slot of the copy that names it.
    pub sources: Vec<RootSource>,
    pub tree_root: RootPointer,
    /// The superblock the candidate's layout comes from: the copy that
    /// named it first.
    pub superblock: Superblock,
    /// Whether that copy verified.
    pub verified: bool,
}

/// Every root tree the superblock copies name, newest generation first:
/// each copy's own root, then its backup slots. A restore-style search
/// tries them in this order, stopping at the first that leads where it
/// needs to go.
///
/// A copy whose checksum fails still contributes its roots, after the
/// verified copies' roots of the same generation; a copy that did not
/// decode contributes nothing.
pub fn root_tree_candidates(copies: &[SuperblockCopy]) -> Vec<RootTreeCandidate> {
    let mut candidates: Vec<RootTreeCandidate> = Vec::new();
    let mut ordered: Vec<&SuperblockCopy> = copies.iter().collect();
    ordered.sort_by_key(|copy| copy.verdict != CopyVerdict::Valid);
    for copy in ordered {
        let Some(sb) = &copy.superblock else {
            continue;
        };
        let verified = copy.verdict == CopyVerdict::Valid;
        let mut named = vec![(
            RootSource::Superblock {
                mirror: copy.mirror,
            },
            RootPointer {
                bytenr: sb.root,
                generation: Some(sb.generation),
                level: Some(sb.root_level),
            },
        )];
        for (slot, backup) in copy.backups.iter().enumerate() {
            if backup.is_empty() {
                continue;
            }
            named.push((
                RootSource::Backup {
                    mirror: copy.mirror,
                    slot,
                },
                RootPointer {
                    bytenr: backup.tree.bytenr,
                    generation: Some(backup.tree.generation),
                    level: Some(backup.tree.level),
                },
            ));
        }
        for (source, tree_root) in named {
            match candidates.iter_mut().find(|c| c.tree_root == tree_root) {
                Some(existing) => existing.sources.push(source),
                None => candidates.push(RootTreeCandidate {
                    sources: vec![source],
                    tree_root,
                    superblock: sb.clone(),
                    verified,
                }),
            }
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.tree_root.generation));
    candidates
}
