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
            Self::read_logical_pool(&self.device, &self.devices, &self.map, logical, buf)
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
    ///
    /// Measured against a real filesystem, not counted from the struct:
    /// the ROOT_ITEM for the extent tree holds the address the
    /// superblock's own walk reaches.
    pub const BYTENR: usize = 176;
    /// The transaction that wrote that root.
    ///
    /// AT 160, after the embedded `btrfs_inode_item`. Offset 16 is
    /// inside that inode and holds something else entirely — a
    /// ROOT_ITEM whose generation was written there leaves the real
    /// field stale, and the kernel refuses the tree it names with
    /// "parent transid verify failed". Which is exactly what `btrfs
    /// check` said before this was measured.
    pub const GENERATION: usize = 160;
    /// Its height, after `drop_progress` and `drop_level`.
    pub const LEVEL: usize = 238;
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

                    // An extent tree leaf carries the record of what is
                    // allocated, so a relocation changes its CONTENTS as
                    // well as its address: every block the plan moves
                    // loses the item naming where it was and gains one
                    // naming where it went. Without this the tree is
                    // correct and the extent tree describes a filesystem
                    // that no longer exists.
                    if rewrite.owner == objectid::EXTENT_TREE {
                        owned = self.apply_records(rewrite.old, owned, plan, generation)?;
                    }

                    // The free-space tree is the complement of the
                    // extent tree, so a transaction changes both. A leaf
                    // left saying where things used to be is what `btrfs
                    // check` calls "cache appears valid but isn't".
                    if rewrite.owner == objectid::FREE_SPACE_TREE {
                        owned = self.apply_free_space(owned, plan)?;
                    }

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

            let before = seed.len();
            seed.extend(self.extent_leaves_for(&touched)?);
            // The free-space tree tracks the same moves from the other
            // side, so its leaves for those addresses are dirty too.
            seed.extend(self.free_space_leaves_for(&touched)?);
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
    /// "Would hold" is the important half. An insert dirties the leaf it
    /// lands in even though nothing is filed under that key yet, and
    /// which leaf that is follows the same rule a descent uses: the LAST
    /// leaf whose first key is not greater than the one being inserted.
    ///
    /// A range test — is the address between this leaf's first and last
    /// key — is not that rule, and gets the common case wrong. A newly
    /// allocated address is usually PAST every key in the tree, so it
    /// falls in no leaf's range, no leaf is dirtied, and the record is
    /// never written. The block then has no back reference and `btrfs
    /// check` says so: "tree extent[...] has no backref item in extent
    /// tree". It only showed up on a fixture whose free space happened
    /// to lie beyond the last record rather than among them.
    fn extent_leaves_for(&self, addresses: &[u64]) -> Result<BTreeSet<u64>> {
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        self.leaves_holding(root, addresses)
    }

    /// The leaves of `root` that an insert of each address would land
    /// in.
    fn leaves_holding(&self, root: u64, addresses: &[u64]) -> Result<BTreeSet<u64>> {
        if addresses.is_empty() {
            return Ok(BTreeSet::new());
        }

        // Every leaf, by its first key.
        let mut leaves: Vec<(u64, u64)> = Vec::new();
        self.for_each_tree_block(root, &mut |at, block, level, _| {
            if level != 0 {
                return;
            }
            let n = u32::from_le_bytes(block[o::NRITEMS..o::NRITEMS + 4].try_into().unwrap());
            if n == 0 {
                return;
            }
            let first = u64::from_le_bytes(block[HEADER_SIZE..HEADER_SIZE + 8].try_into().unwrap());
            leaves.push((first, at));
        })?;
        if leaves.is_empty() {
            return Ok(BTreeSet::new());
        }
        leaves.sort();

        let mut out = BTreeSet::new();
        for a in addresses {
            // The last leaf that begins at or before this key, or the
            // first leaf when the key precedes everything.
            let idx = match leaves.binary_search_by(|(first, _)| first.cmp(a)) {
                Ok(i) => i,
                Err(0) => 0,
                Err(i) => i - 1,
            };
            out.insert(leaves[idx].1);
        }
        Ok(out)
    }
}

impl Filesystem {
    /// Apply a plan's allocations and releases to one extent tree leaf.
    ///
    /// Each moved block loses the `METADATA_ITEM` naming its old address
    /// and gains one naming the new. Only the items belonging in THIS
    /// leaf are touched: which leaf an address belongs to is decided by
    /// the key range, and every leaf that any of them falls into is in
    /// the plan — that is what closing the plan over its own bookkeeping
    /// guarantees.
    ///
    /// # Errors
    ///
    /// Propagates a refusal from the leaf editor. An address whose
    /// record is missing is an error rather than a skip: it means the
    /// extent tree does not say what the plan believes, and carrying on
    /// would leave a block recorded as allocated for ever.
    fn apply_records(
        &self,
        leaf: u64,
        items: Vec<crate::leaf_edit::OwnedItem>,
        plan: &Plan,
        generation: u64,
    ) -> Result<Vec<crate::leaf_edit::OwnedItem>> {
        use crate::extent_write::{record_tree_block, TreeBlockAllocation};
        use crate::leaf_edit::{delete, insert, OwnedItem};

        // Which leaf each address belongs to, by the rule a descent
        // uses — NOT by whether this leaf's existing keys bracket it. A
        // newly allocated address is usually past every key in the tree,
        // and a bracket test skips it: the record is never written and
        // `btrfs check` reports the block as having no backref item.
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        let mine =
            |at: u64| -> Result<bool> { Ok(self.leaves_holding(root, &[at])?.contains(&leaf)) };

        let mut out = items;

        // Releases first, so the leaf is at its smallest before
        // anything is added to it.
        for rewrite in &plan.rewrites {
            if !mine(rewrite.old)? {
                continue;
            }
            let key = TreeBlockAllocation {
                bytenr: rewrite.old,
                level: rewrite.level,
                generation,
                owner: rewrite.owner,
            }
            .key();
            if out.iter().any(|i| i.key == key) {
                out = delete(&out, &key)?;
            }
        }

        for rewrite in &plan.rewrites {
            if !mine(rewrite.new)? {
                continue;
            }
            let alloc = TreeBlockAllocation {
                bytenr: rewrite.new,
                level: rewrite.level,
                generation,
                owner: rewrite.owner,
            };
            let (key, body) = record_tree_block(&self.sb, alloc)?;
            if out.iter().any(|i| i.key == key) {
                continue;
            }
            out = insert(
                self.sb.nodesize,
                &out,
                OwnedItem {
                    key,
                    data: body.to_vec(),
                },
            )?;
        }
        Ok(out)
    }
}

/// `BTRFS_FREE_SPACE_INFO_KEY` / `..._EXTENT_KEY` / `..._BITMAP_KEY`.
const FREE_SPACE_INFO_KEY: u8 = 198;
const FREE_SPACE_EXTENT_KEY: u8 = 199;
const FREE_SPACE_BITMAP_KEY: u8 = 200;

impl Filesystem {
    /// The free-space tree leaves that describe any of `addresses`.
    fn free_space_leaves_for(&self, addresses: &[u64]) -> Result<BTreeSet<u64>> {
        let Ok(root) = self.tree_root(objectid::FREE_SPACE_TREE) else {
            return Ok(BTreeSet::new());
        };
        let groups = self.block_groups()?;
        let spans: Vec<(u64, u64)> = groups
            .iter()
            .filter(|g| addresses.iter().any(|a| g.contains(*a)))
            .map(|g| (g.start, g.end()))
            .collect();
        if spans.is_empty() {
            return Ok(BTreeSet::new());
        }

        // Same rule as the extent tree: an insert lands in the last
        // leaf that begins at or before its key, which is not the same
        // as the leaf whose existing keys bracket it.
        let mut keys: Vec<u64> = Vec::new();
        for (start, end) in &spans {
            keys.push(*start);
            for a in addresses.iter().filter(|a| **a >= *start && **a < *end) {
                keys.push(*a);
            }
        }
        self.leaves_holding(root, &keys)
    }

    /// Rewrite a free-space tree leaf so it describes what the plan
    /// leaves behind.
    ///
    /// The free-space tree is the complement of the extent tree, so a
    /// transaction that moves blocks changes both. A leaf left saying
    /// where things used to be is what `btrfs check` reports as "cache
    /// appears valid but isn't".
    ///
    /// # The two things a first attempt got wrong
    ///
    /// **A leaf is not per-block-group.** Measured: the whole tree can
    /// be one leaf holding a `FREE_SPACE_INFO` for each of several
    /// groups, each followed by that group's `FREE_SPACE_EXTENT`s in
    /// objectid order. So a leaf is rewritten group by group, and every
    /// group in it must come out again.
    ///
    /// **An `INFO` may name a group that no longer exists.** The tree
    /// outlives its block groups and `btrfs check` calls that correct —
    /// see `docs/cow-transaction.md`. Those entries are carried through
    /// untouched: there is no block group to derive a free set from, and
    /// dropping them would delete something the kernel put there.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] for a leaf holding a
    /// `FREE_SPACE_BITMAP`, which needs its bits rewritten rather than
    /// its extents and is not implemented. Refusing beats writing
    /// extents where the kernel will read bits.
    fn apply_free_space(
        &self,
        items: Vec<crate::leaf_edit::OwnedItem>,
        plan: &Plan,
    ) -> Result<Vec<crate::leaf_edit::OwnedItem>> {
        use crate::block_group::FreeExtent;
        use crate::chunk::DiskKey;
        use crate::leaf_edit::OwnedItem;

        if items
            .iter()
            .any(|i| i.key.key_type == FREE_SPACE_BITMAP_KEY)
        {
            return Err(Error::UnsupportedFeature(
                "this free-space tree records a block group as a bitmap, and rewriting \
                 bits is not implemented"
                    .to_string(),
            ));
        }

        let groups = self.block_groups()?;
        let released: Vec<u64> = plan.released();
        let allocated: Vec<u64> = plan.allocated();
        let nodesize = self.sb.nodesize as u64;

        let mut out: Vec<OwnedItem> = Vec::with_capacity(items.len());
        let mut i = 0usize;
        while i < items.len() {
            let item = &items[i];
            if item.key.key_type != FREE_SPACE_INFO_KEY {
                // An extent item with no info before it: carry it.
                out.push(item.clone());
                i += 1;
                continue;
            }

            // This group's span, and the run of items belonging to it.
            let start = item.key.objectid;
            let end = start + item.key.offset;
            let mut j = i + 1;
            while j < items.len()
                && items[j].key.key_type != FREE_SPACE_INFO_KEY
                && items[j].key.objectid < end
            {
                j += 1;
            }

            let touched = released
                .iter()
                .chain(allocated.iter())
                .any(|a| *a >= start && *a < end);
            let group = groups.iter().find(|g| g.start == start);

            match (touched, group) {
                // Untouched, or a group that no longer exists: carry the
                // whole run through exactly as it was.
                (false, _) | (_, None) => out.extend_from_slice(&items[i..j]),
                (true, Some(group)) => {
                    // What is free now, plus what the plan releases,
                    // minus what it takes.
                    let mut free = self.free_extents(group)?;
                    for at in released.iter().filter(|a| group.contains(**a)) {
                        free.push(FreeExtent {
                            start: *at,
                            len: nodesize,
                        });
                    }
                    free.sort();

                    let mut merged: Vec<FreeExtent> = Vec::with_capacity(free.len());
                    for run in free {
                        match merged.last_mut() {
                            Some(prev) if prev.end() == run.start => prev.len += run.len,
                            _ => merged.push(run),
                        }
                    }

                    let mut runs: Vec<FreeExtent> = merged;
                    for at in allocated.iter().filter(|a| group.contains(**a)) {
                        runs = runs
                            .into_iter()
                            .flat_map(|r| carve(r, *at, nodesize))
                            .collect();
                    }
                    runs.retain(|r| r.len > 0);

                    let mut info = item.clone();
                    if info.data.len() >= 4 {
                        info.data[0..4].copy_from_slice(&(runs.len() as u32).to_le_bytes());
                    }
                    out.push(info);
                    for run in runs {
                        out.push(OwnedItem {
                            key: DiskKey {
                                objectid: run.start,
                                key_type: FREE_SPACE_EXTENT_KEY,
                                offset: run.len,
                            },
                            data: Vec::new(),
                        });
                    }
                }
            }
            i = j;
        }
        Ok(out)
    }
}

/// `run` with `[at, at + len)` taken out of it.
fn carve(
    run: crate::block_group::FreeExtent,
    at: u64,
    len: u64,
) -> Vec<crate::block_group::FreeExtent> {
    use crate::block_group::FreeExtent;
    let end = at + len;
    if end <= run.start || at >= run.end() {
        return vec![run];
    }
    let mut out = Vec::new();
    if at > run.start {
        out.push(FreeExtent {
            start: run.start,
            len: at - run.start,
        });
    }
    if end < run.end() {
        out.push(FreeExtent {
            start: end,
            len: run.end() - end,
        });
    }
    out
}
