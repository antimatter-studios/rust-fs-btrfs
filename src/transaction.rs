//! Which blocks a change makes the filesystem rewrite.
//!
//! Copy-on-write means nothing is modified where it lies. Changing one
//! byte of one leaf means a new leaf somewhere else, a new node above it
//! pointing there, and so on to the root — then the root tree, which
//! names that tree's root and has therefore changed too. This works out
//! that set.
//!
//! # Why it is not just "walk up to the root"
//!
//! Because the walk does not stop at the root. `docs/cow-transaction.md`
//! measured a real transaction: a filesystem where a mount cycle changed
//! NOTHING still rewrote four blocks — the root tree, the extent tree,
//! the free-space tree and the dev tree — and swapped four
//! `METADATA_ITEM`s, four in and four out.
//!
//! That is the recursion, visible. Rewriting a block means allocating
//! one, allocating means recording it in the extent tree, and the extent
//! tree lives in blocks that must themselves be allocated. It terminates
//! because a copy-on-write rewrite is an allocation AND a release, so the
//! extent tree ends up recording its own new blocks rather than growing
//! without bound.
//!
//! # What this computes, and what it does not
//!
//! It computes the closure: given blocks whose contents changed, every
//! block that must be rewritten as a consequence, and a new address for
//! each. That is the part the measurement pinned down.
//!
//! It does not EDIT the extent tree. Adding and removing the
//! `METADATA_ITEM`s that record the plan means inserting into and
//! deleting from a leaf, with splits and merges when one fills or
//! empties, and none of that is implemented. So a plan says what a
//! transaction would cost and where everything would go; it does not yet
//! produce the item changes that make it true.
//!
//! That boundary is deliberate. The plan is checkable against what the
//! kernel actually did — see `tests/transaction_plan.rs` — and being
//! checkable before it is complete is worth more than being complete and
//! unverified.

use crate::btree::{header_offsets as o, HEADER_SIZE};
use crate::chunk::objectid;
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use std::collections::{BTreeMap, BTreeSet};

/// One block moving from where it is to where it will be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rewrite {
    /// Where it is now. Released once the transaction commits.
    pub old: u64,
    /// Where the new copy goes.
    pub new: u64,
    /// The tree it belongs to.
    pub owner: u64,
    /// Its height above the leaves.
    pub level: u8,
}

/// What a transaction will do.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Every block that must be written, in no particular order — the
    /// commit sequencer writes them all before its barrier, so the order
    /// among them carries no meaning.
    pub rewrites: Vec<Rewrite>,
}

impl Plan {
    /// The addresses this frees.
    pub fn released(&self) -> Vec<u64> {
        self.rewrites.iter().map(|r| r.old).collect()
    }

    /// The addresses this takes.
    pub fn allocated(&self) -> Vec<u64> {
        self.rewrites.iter().map(|r| r.new).collect()
    }

    /// Which trees the transaction touches, in objectid order.
    pub fn trees(&self) -> BTreeSet<u64> {
        self.rewrites.iter().map(|r| r.owner).collect()
    }

    /// How much `bytes_used` moves.
    ///
    /// Zero for any plan that rewrites blocks without adding or removing
    /// any, which is every plan this produces — one release for every
    /// allocation. It is a method rather than a constant because that
    /// stops being true the moment a leaf split is implemented, and a
    /// caller should be asking rather than assuming.
    pub fn usage_delta(&self, nodesize: u64) -> i128 {
        (self.allocated().len() as i128 - self.released().len() as i128) * nodesize as i128
    }
}

/// What a tree walk hands to its visitor: a block's address, its bytes,
/// its level, and the node that points at it — `None` for a root.
type BlockVisitor<'a> = &'a mut dyn FnMut(u64, &[u8], u8, Option<u64>);

/// A block's place in the tree it belongs to.
#[derive(Debug, Clone, Copy)]
struct Placement {
    parent: Option<u64>,
    owner: u64,
    level: u8,
}

impl Filesystem {
    /// Work out every block that must be rewritten if `dirty` change.
    ///
    /// The result includes `dirty` itself, every ancestor of each up to
    /// its tree's root, and the root tree's own path — because when a
    /// tree's root moves, the `ROOT_ITEM` naming it has changed, and the
    /// leaf holding that item is itself a block that must be rewritten.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] if a block in `dirty` is not part
    /// of any tree this can reach, which means it is not a tree block or
    /// is not live — planning around it would allocate for something
    /// nothing points at.
    ///
    /// Propagates an allocation failure when there is nowhere to put the
    /// new copies.
    pub fn plan_transaction(&self, dirty: &[u64]) -> Result<Plan> {
        let places = self.placements()?;

        // The closure: everything dirty, plus every ancestor, plus the
        // root tree's path to whichever ROOT_ITEM leaf named a root that
        // moved.
        let mut touched: BTreeSet<u64> = BTreeSet::new();
        let mut queue: Vec<u64> = dirty.to_vec();

        while let Some(at) = queue.pop() {
            if !touched.insert(at) {
                continue;
            }
            let place = places.get(&at).ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "the block at {at} is not reachable from any tree, so there is nothing \
                     above it to rewrite"
                ))
            })?;

            match place.parent {
                // Not a root: its parent points at it and must change.
                Some(parent) => queue.push(parent),
                // A tree's root. If it is not the ROOT TREE's own root,
                // the root tree holds a ROOT_ITEM naming it, and that
                // leaf has changed.
                None if place.owner != objectid::ROOT_TREE => {
                    if let Some(leaf) = self.root_item_leaf(place.owner)? {
                        queue.push(leaf);
                    }
                }
                None => {}
            }
        }

        // A new home for each, all distinct and none of them somewhere
        // already in use.
        let mut plan = Plan::default();
        let mut taken: BTreeSet<u64> = BTreeSet::new();
        for old in touched {
            let place = places[&old];
            let new = self.next_free_block(&taken)?;
            taken.insert(new);
            plan.rewrites.push(Rewrite {
                old,
                new,
                owner: place.owner,
                level: place.level,
            });
        }
        Ok(plan)
    }

    /// Somewhere to put a block that is not already spoken for.
    ///
    /// [`Filesystem::find_metadata_block`] does not record what it hands
    /// out, so asking twice gives the same answer twice. Until a
    /// transaction records its allocations, the addresses it has already
    /// chosen are held here.
    fn next_free_block(&self, taken: &BTreeSet<u64>) -> Result<u64> {
        let nodesize = self.sb.nodesize as u64;
        let groups: Vec<_> = self
            .block_groups()?
            .into_iter()
            .filter(|g| g.holds_metadata())
            .collect();

        for runs in self.free_extents_by_group(&groups)? {
            for run in runs {
                let mut at = run.start.next_multiple_of(nodesize);
                while at + nodesize <= run.end() {
                    if !taken.contains(&at) {
                        return Ok(at);
                    }
                    at += nodesize;
                }
            }
        }
        Err(Error::UnsupportedFeature(format!(
            "no metadata block group has room for another {nodesize}-byte block; \
             allocating a new block group is not implemented"
        )))
    }

    /// The root tree leaf holding the `ROOT_ITEM` for `objectid`.
    fn root_item_leaf(&self, objectid: u64) -> Result<Option<u64>> {
        /// `BTRFS_ROOT_ITEM_KEY`.
        const ROOT_ITEM_KEY: u8 = 132;
        let mut found = None;
        self.for_each_tree_block(self.sb.root, &mut |at, block, level, _| {
            if level != 0 || found.is_some() {
                return;
            }
            let n = u32::from_le_bytes(block[o::NRITEMS..o::NRITEMS + 4].try_into().unwrap());
            for i in 0..n as usize {
                let it = HEADER_SIZE + i * 25;
                if it + 25 > block.len() {
                    break;
                }
                let oid = u64::from_le_bytes(block[it..it + 8].try_into().unwrap());
                if oid == objectid && block[it + 8] == ROOT_ITEM_KEY {
                    found = Some(at);
                    return;
                }
            }
        })?;
        Ok(found)
    }

    /// Where every reachable block sits: its parent, tree and level.
    fn placements(&self) -> Result<BTreeMap<u64, Placement>> {
        /// `BTRFS_ROOT_ITEM_KEY`.
        const ROOT_ITEM_KEY: u8 = 132;
        /// `btrfs_root_item.bytenr`.
        const ROOT_ITEM_BYTENR: usize = 176;

        let mut out = BTreeMap::new();
        let mut roots = vec![self.sb.root];

        while let Some(root) = roots.pop() {
            if root == 0 {
                continue;
            }
            let mut found_roots = Vec::new();
            self.for_each_tree_block(root, &mut |at, block, level, parent| {
                let owner = u64::from_le_bytes(block[o::OWNER..o::OWNER + 8].try_into().unwrap());
                out.insert(
                    at,
                    Placement {
                        parent,
                        owner,
                        level,
                    },
                );

                if level != 0 || owner != objectid::ROOT_TREE {
                    return;
                }
                // A root tree leaf: each ROOT_ITEM names another tree.
                let n = u32::from_le_bytes(block[o::NRITEMS..o::NRITEMS + 4].try_into().unwrap());
                for i in 0..n as usize {
                    let it = HEADER_SIZE + i * 25;
                    if it + 25 > block.len() || block[it + 8] != ROOT_ITEM_KEY {
                        continue;
                    }
                    let off = HEADER_SIZE
                        + u32::from_le_bytes(block[it + 17..it + 21].try_into().unwrap()) as usize;
                    if off + ROOT_ITEM_BYTENR + 8 <= block.len() {
                        let b = u64::from_le_bytes(
                            block[off + ROOT_ITEM_BYTENR..off + ROOT_ITEM_BYTENR + 8]
                                .try_into()
                                .unwrap(),
                        );
                        if b != 0 && !out.contains_key(&b) {
                            found_roots.push(b);
                        }
                    }
                }
            })?;
            roots.extend(found_roots);
        }
        Ok(out)
    }

    /// Walk a tree, handing each block to `visit` with its parent.
    ///
    /// `visit` receives the block's address, its bytes, its level, and
    /// the address of the node that points at it — `None` for a root.
    ///
    /// # Errors
    ///
    /// Propagates a read failure. A block that will not read is skipped
    /// rather than fatal: a tree this driver cannot fully walk is still
    /// one whose reachable part is worth knowing.
    fn for_each_tree_block(&self, root: u64, visit: BlockVisitor) -> Result<()> {
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical(&self.device, &self.map, logical, buf)
        };
        let tree = crate::btree::Tree::from_superblock(&self.sb, &read);

        let mut stack = vec![(root, None)];
        let mut seen = BTreeSet::new();
        while let Some((at, parent)) = stack.pop() {
            if at == 0 || !seen.insert(at) {
                continue;
            }
            let Ok(block) = tree.read_block(at) else {
                continue;
            };
            let level = block.header.level;
            visit(at, block.bytes(), level, parent);
            if let Some(ptrs) = block.body.key_ptrs() {
                for p in ptrs {
                    stack.push((p.blockptr, Some(at)));
                }
            }
        }
        Ok(())
    }
}

/// Byte offsets inside a `btrfs_root_item`, of the fields a relocation
/// has to update.
mod root_item {
    /// The tree's root address.
    pub const BYTENR: usize = 176;
    /// The transaction that wrote it.
    pub const GENERATION: usize = 16;
    /// Its height.
    pub const LEVEL: usize = 246;
}

/// `BTRFS_ROOT_ITEM_KEY`.
const ROOT_ITEM_KEY: u8 = 132;

impl Filesystem {
    /// Turn a plan into the blocks it says to write.
    ///
    /// Every block keeps its contents and changes its address. What that
    /// means depends on what the block is:
    ///
    /// - a **leaf** keeps its items exactly, and is re-stamped with its
    ///   new address and the new generation;
    /// - a **node** keeps its keys, but every child that also moved is
    ///   pointed at its new address — a node still naming the old one is
    ///   a tree that reads the version before the change;
    /// - a **root tree leaf** additionally has its `ROOT_ITEM`s
    ///   rewritten, because those name the roots of other trees, and a
    ///   tree whose root moved has a stale `ROOT_ITEM` otherwise.
    ///
    /// The result goes straight to [`Filesystem::commit`].
    ///
    /// # What it does not do
    ///
    /// It does not add or remove anything. A relocation is the part of a
    /// transaction that moves what is already there; the item changes
    /// that record the moves in the extent tree are separate and are not
    /// produced here. So a filesystem committed from this alone has a
    /// correct tree and an extent tree that still describes the old
    /// addresses.
    ///
    /// # Errors
    ///
    /// Propagates a read failure, and [`Error::UnsupportedFeature`] if a
    /// block cannot be re-encoded at its new size — which cannot happen
    /// for a straight relocation and is reported rather than assumed
    /// away.
    pub fn render_plan(
        &self,
        plan: &Plan,
        generation: u64,
    ) -> Result<Vec<crate::commit::PlacedBlock>> {
        use crate::commit::PlacedBlock;
        use crate::leaf_edit::OwnedItem;
        use crate::tree_write::{build_leaf, build_node, chunk_tree_uuid_of, BlockIdentity};

        // Where each moved block is going.
        let moved: BTreeMap<u64, u64> = plan.rewrites.iter().map(|r| (r.old, r.new)).collect();

        let mut out = Vec::with_capacity(plan.rewrites.len());
        for rewrite in &plan.rewrites {
            let block = self.read_tree_block(rewrite.old)?;
            let raw = block.bytes().to_vec();
            let id = BlockIdentity {
                bytenr: rewrite.new,
                owner: rewrite.owner,
                generation,
                level: rewrite.level,
                flags: crate::tree_write::flags_for_new_block(),
                chunk_tree_uuid: chunk_tree_uuid_of(&raw),
            };

            let bytes = match block.body.key_ptrs() {
                Some(ptrs) => {
                    // A node: follow every child that moved.
                    let updated: Vec<_> = ptrs
                        .iter()
                        .map(|p| {
                            let mut p = *p;
                            if let Some(&to) = moved.get(&p.blockptr) {
                                p.blockptr = to;
                                p.generation = generation;
                            }
                            p
                        })
                        .collect();
                    build_node(&self.sb, id, &updated)?
                }
                None => {
                    let items = block.body.items().unwrap_or(&[]);
                    let mut owned: Vec<OwnedItem> = items
                        .iter()
                        .filter_map(|item| {
                            block.item_data(item).map(|data| OwnedItem {
                                key: item.key,
                                data: data.to_vec(),
                            })
                        })
                        .collect();

                    // A root tree leaf names other trees' roots.
                    if rewrite.owner == objectid::ROOT_TREE {
                        for item in &mut owned {
                            if item.key.key_type != ROOT_ITEM_KEY
                                || item.data.len() < root_item::LEVEL + 1
                            {
                                continue;
                            }
                            let at = u64::from_le_bytes(
                                item.data[root_item::BYTENR..root_item::BYTENR + 8]
                                    .try_into()
                                    .expect("8 bytes"),
                            );
                            if let Some(&to) = moved.get(&at) {
                                item.data[root_item::BYTENR..root_item::BYTENR + 8]
                                    .copy_from_slice(&to.to_le_bytes());
                                item.data[root_item::GENERATION..root_item::GENERATION + 8]
                                    .copy_from_slice(&generation.to_le_bytes());
                            }
                        }
                    }

                    let borrowed: Vec<_> = owned.iter().map(|i| i.as_leaf_item()).collect();
                    build_leaf(&self.sb, id, &borrowed)?
                }
            };

            out.push(PlacedBlock {
                logical: rewrite.new,
                bytes,
            });
        }
        Ok(out)
    }

    /// Where the root tree ends up, for a plan that moves it.
    ///
    /// The superblock names the root tree, so committing a plan needs
    /// this. Returns `None` when the plan does not move the root tree,
    /// which means the superblock's existing value still stands.
    pub fn planned_root(&self, plan: &Plan) -> Option<u64> {
        plan.rewrites
            .iter()
            .find(|r| r.old == self.sb.root)
            .map(|r| r.new)
    }
}

impl Filesystem {
    /// Plan a transaction including the extent tree's own rewrite.
    ///
    /// [`Filesystem::plan_transaction`] computes the spine: the dirty
    /// blocks, their ancestors, and the root tree leaf naming a tree
    /// whose root moved. That is not a whole transaction, because moving
    /// a block means recording the move — a `METADATA_ITEM` added for
    /// the new address and the old one's removed — and those items live
    /// in extent tree leaves, which are themselves blocks that must then
    /// be rewritten.
    ///
    /// That is the recursion `docs/cow-transaction.md` measured, where a
    /// commit changing NOTHING still rewrote four blocks. It terminates
    /// because the extent tree ends up recording its own new blocks
    /// rather than growing: each rewrite is an allocation and a release.
    ///
    /// This closes it by iterating. Each round works out which extent
    /// tree leaves hold the records for the blocks moved so far, adds
    /// them to the dirty set, and plans again. When a round adds nothing
    /// the plan is closed.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::plan_transaction`], and
    /// [`Error::UnsupportedFeature`] if the iteration does not settle
    /// within `rounds`. That is reported rather than papered over: a
    /// transaction whose own bookkeeping keeps producing more work is
    /// one that needs the kernel's reservation machinery, not another
    /// turn of the loop.
    pub fn plan_transaction_closed(&self, dirty: &[u64], rounds: usize) -> Result<Plan> {
        let mut seed: BTreeSet<u64> = dirty.iter().copied().collect();

        for _ in 0..rounds {
            let list: Vec<u64> = seed.iter().copied().collect();
            let plan = self.plan_transaction(&list)?;

            // Every address whose record changes: the old ones lose a
            // METADATA_ITEM, the new ones gain one.
            let mut touched: Vec<u64> = plan.released();
            touched.extend(plan.allocated());

            let leaves = self.extent_leaves_for(&touched)?;
            let before = seed.len();
            seed.extend(leaves);
            if seed.len() == before {
                return Ok(plan);
            }
        }

        Err(Error::UnsupportedFeature(format!(
            "the transaction did not settle in {rounds} rounds: recording each round's \
             moves keeps dirtying extent tree leaves that were not already in it. \
             Breaking that needs reservations rather than another iteration"
        )))
    }

    /// The extent tree leaves that hold, or would hold, the records for
    /// `addresses`.
    ///
    /// A leaf "would hold" an address when the address falls inside its
    /// key range: that is where an insert lands, so it is the leaf an
    /// insert dirties even though nothing is filed under that key yet.
    fn extent_leaves_for(&self, addresses: &[u64]) -> Result<BTreeSet<u64>> {
        if addresses.is_empty() {
            return Ok(BTreeSet::new());
        }
        let root = self.tree_root(objectid::EXTENT_TREE)?;

        let mut out = BTreeSet::new();
        self.for_each_tree_block(root, &mut |at, block, level, _| {
            if level != 0 {
                return;
            }
            let n = u32::from_le_bytes(block[o::NRITEMS..o::NRITEMS + 4].try_into().unwrap());
            if n == 0 {
                return;
            }
            let first = u64::from_le_bytes(block[HEADER_SIZE..HEADER_SIZE + 8].try_into().unwrap());
            let last_at = HEADER_SIZE + (n as usize - 1) * 25;
            let last = u64::from_le_bytes(block[last_at..last_at + 8].try_into().unwrap());

            if addresses.iter().any(|a| *a >= first && *a <= last) {
                out.insert(at);
            }
        })?;
        Ok(out)
    }
}
