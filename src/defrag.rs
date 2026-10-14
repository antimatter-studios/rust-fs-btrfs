//! Defragmenting a file: each run of its extents rewritten as one (#305).
//!
//! A file written a little at a time, alongside other files, ends up as
//! many small extents scattered across the volume. Defragmenting copies
//! each run of them into one newly allocated extent, through the same
//! copy-on-write path as a write ([`crate::cow_write`]): the run's first
//! `EXTENT_DATA` item is pointed at the copy and made to cover the whole
//! run, the items after it are deleted, the extent tree records the copy
//! with one reference and gives up the old extents, the free-space tree,
//! the block groups' `used` and the superblock's `bytes_used` follow, and
//! the checksum tree swaps the old extents' digests for a digest per
//! sector of the copy (unless the file is `nodatasum`). One committed
//! transaction for the whole file, so a crash leaves the file as it was
//! or as it ends up, and never between.
//!
//! The bytes the file reads do not change, and nor do its times or its
//! `nbytes`: only where the bytes lie, and in how many extents.
//!
//! # Which extents move
//!
//! A run is two or more adjacent items — each beginning where the last
//! ends — that each:
//!
//! - are regular, with an extent behind them: not inline, not
//!   preallocated, not a hole;
//! - are stored unencoded: a compressed extent is left as it is, since a
//!   copy of several would be stored uncompressed;
//! - are shorter than the target size, `btrfs filesystem defragment
//!   -t`'s meaning: an extent at least that long is left alone;
//! - reference an extent no other item of the file references, since
//!   giving up such an extent means dropping every one of those
//!   references in one change.
//!
//! A run ends at the item that would take it past the target, and the
//! next run begins there. The default target is
//! [`DEFRAG_TARGET`], 32 MiB, as `btrfs filesystem defragment`'s is; no
//! target is longer than [`MAX_EXTENT`], the largest data extent Btrfs
//! makes. Copies are stored uncompressed, as a write's copy longer than a
//! compressed extent may hold is.
//!
//! # Shared extents
//!
//! An extent a reflink shares is copied like any other, and this file's
//! reference to it is dropped: the other files keep reading the old
//! extent, and this one its own copy. That is what the kernel does by
//! default — defragmenting breaks reflinks, which is why it can take more
//! space than it frees.
//!
//! # What it refuses
//!
//! - an extent from at or before the last snapshot of the subvolume,
//!   which the snapshot may still be reading: the copy-on-write write
//!   refuses the same extent, for the same reason;
//! - every refusal of the transaction it commits through: a reference to
//!   a shared extent held other than inline in its record, a tree leaf
//!   the change would empty, a free-space tree recorded as bitmaps, and a
//!   run with no contiguous free space as long as it in any data block
//!   group.
//!
//! Nothing is written unless all of it is.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::chunk::{objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::{file_extent as fe, Filesystem, EXTENT_DATA_KEY, EXTENT_REGULAR};
use crate::inode::{offsets as io, INODE_ITEM_KEY, INODE_NODATASUM};
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::transaction::{DataMove, DataRelease, DataWrite, ItemEdit};
use crate::write::window_inside_extent;

/// The target extent size when none is given: 32 MiB, the default of
/// `btrfs filesystem defragment -t`.
pub const DEFRAG_TARGET: u64 = 32 << 20;

/// The longest data extent Btrfs makes, `BTRFS_MAX_EXTENT_SIZE`: no
/// target is taken past it.
pub const MAX_EXTENT: u64 = 128 << 20;

/// How many rounds the change's plan has to close over its own
/// bookkeeping, as for a write.
const PLAN_ROUNDS: usize = 64;

/// What a defragment did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Defragmented {
    /// The file's `EXTENT_DATA` items before, holes and inline data
    /// included.
    pub items_before: usize,
    /// And after: each run rewritten is one item.
    pub items_after: usize,
    /// How many runs were rewritten, each into one new extent.
    pub runs: usize,
    /// The bytes copied into them.
    pub bytes: u64,
}

/// One `EXTENT_DATA` item, as the run finder sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Item {
    /// Its key offset: where in the file it begins.
    start: u64,
    /// How much of the file it covers, its `num_bytes`.
    len: u64,
    /// Whether it may join a run: see the module documentation.
    movable: bool,
}

/// The runs of `items` worth rewriting, as index ranges: two or more
/// adjacent movable items, each beginning where the last ends, together
/// no longer than `target`.
fn runs(items: &[Item], target: u64) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < items.len() {
        if !items[i].movable || items[i].len >= target {
            i += 1;
            continue;
        }
        let begin = i;
        let mut total = items[i].len;
        let mut j = i + 1;
        while j < items.len() {
            let (prev, next) = (items[j - 1], items[j]);
            let joins = next.movable
                && prev.start.checked_add(prev.len) == Some(next.start)
                && total.checked_add(next.len).is_some_and(|t| t <= target);
            if !joins {
                break;
            }
            total += next.len;
            j += 1;
        }
        if j - begin >= 2 {
            out.push(begin..j);
        }
        i = j;
    }
    out
}

/// One regular item of a run, with its extent's record.
#[derive(Debug, Clone, Copy)]
struct Member {
    key: DiskKey,
    /// Its `num_bytes`.
    len: u64,
    /// The extent it references, and the extent tree's length for it.
    extent: u64,
    extent_len: u64,
    /// The file offset at which that extent's first byte would sit: the
    /// offset in the extent's `EXTENT_DATA_REF`.
    ref_offset: u64,
    /// More than one reference: a reflink shares the extent.
    shared: bool,
}

impl Filesystem {
    /// Defragment regular file `ino` to the default target,
    /// [`DEFRAG_TARGET`]: see [`Filesystem::defragment_to`].
    ///
    /// # Errors
    ///
    /// As [`Filesystem::defragment_to`].
    pub fn defragment(&mut self, ino: u64) -> Result<Defragmented> {
        self.defragment_to(ino, DEFRAG_TARGET)
    }

    /// Rewrite each run of `ino`'s extents shorter than `target` as one
    /// extent, as one committed transaction — see the [module
    /// documentation](crate::defrag) for which extents move and what is
    /// refused. A file with no such run is left untouched and nothing is
    /// committed.
    ///
    /// Takes `&mut self` because a commit moves the trees this mount
    /// reads: once it lands, the mount is reopened on the new generation.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but a regular file, and
    /// [`Error::UnsupportedFeature`] for a target shorter than a sector
    /// and for every refusal. If reopening the mount after the commit
    /// fails, the change is committed and the error is returned; this
    /// handle then still reads the previous generation and must be
    /// dropped.
    pub fn defragment_to(&mut self, ino: u64, target: u64) -> Result<Defragmented> {
        self.require_writable_top_level()?;
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() {
            return Err(Error::NotAFile);
        }
        let sector = u64::from(self.sb.sectorsize);
        if target < sector {
            return Err(Error::UnsupportedFeature(format!(
                "a defragment target of {target} bytes is shorter than one {sector}-byte sector"
            )));
        }
        let target = target.min(MAX_EXTENT);

        let all: Vec<(DiskKey, Vec<u8>)> = self
            .items_of(ino)?
            .into_iter()
            .filter(|(k, _)| k.key_type == EXTENT_DATA_KEY)
            .collect();
        // How many of the file's items reference each extent.
        let mut referenced: BTreeMap<u64, usize> = BTreeMap::new();
        for (_, d) in &all {
            if let Some(bytenr) = extent_of(d) {
                *referenced.entry(bytenr).or_default() += 1;
            }
        }
        let items: Vec<Item> = all
            .iter()
            .map(|(k, d)| Item {
                start: k.offset,
                len: num_bytes(d),
                movable: movable(d) && extent_of(d).is_some_and(|b| referenced[&b] == 1),
            })
            .collect();
        let found = runs(&items, target);
        let mut report = Defragmented {
            items_before: all.len(),
            items_after: all.len(),
            ..Defragmented::default()
        };
        if found.is_empty() {
            return Ok(report);
        }

        // Every refusal before anything is allocated.
        let last_snapshot = self.fs_tree_last_snapshot()?;
        let mut members: Vec<Vec<Member>> = Vec::with_capacity(found.len());
        for run in &found {
            let mut out = Vec::with_capacity(run.len());
            for (k, d) in &all[run.clone()] {
                out.push(self.member(ino, k, d, last_snapshot)?);
            }
            members.push(out);
        }

        // Each run's bytes, as the file reads them now. Past the end of
        // the file the last sector reads as zeros, and that is what the
        // copy holds there.
        let mut copies: Vec<Vec<u8>> = Vec::with_capacity(members.len());
        for run in &members {
            let start = run[0].key.offset;
            let len: u64 = run.iter().map(|m| m.len).sum();
            let size = usize::try_from(len)
                .map_err(|_| Error::UnsupportedFeature(format!("a run of {len} bytes")))?;
            let mut buf = vec![0u8; size];
            self.read_at(ino, start, &mut buf)?;
            copies.push(buf);
        }
        let lens: Vec<u64> = copies.iter().map(|c| c.len() as u64).collect();
        let news = self.find_data_extents(&lens)?;
        let contents: Vec<(u64, Vec<u8>)> = news.into_iter().zip(copies).collect();

        let generation = self.next_generation()?;
        let mut moves = Vec::with_capacity(members.len());
        let mut releases = Vec::new();
        let mut unrefs = Vec::new();
        let mut edits = Vec::new();
        for (run, (new, bytes)) in members.iter().zip(&contents) {
            let first = run[0];
            moves.push(DataMove {
                item_len: first.len,
                old_shared: first.shared,
                old: first.extent,
                old_len: first.extent_len,
                old_ref_offset: first.ref_offset,
                new: *new,
                len: bytes.len() as u64,
                disk_len: bytes.len() as u64,
                compression: 0,
                file_offset: first.key.offset,
            });
            for m in run {
                let release = DataRelease {
                    old: m.extent,
                    old_len: m.extent_len,
                    old_ref_offset: m.ref_offset,
                };
                // A shared first item loses its reference through the
                // DataMove; later items lose theirs through these edits.
                if m.shared && m.key != first.key {
                    unrefs.push(release);
                } else if !m.shared && m.key != first.key {
                    releases.push(release);
                }
                if m.key != first.key {
                    edits.push(ItemEdit::Delete(m.key));
                }
            }
            report.runs += 1;
            report.bytes += bytes.len() as u64;
            report.items_after -= run.len() - 1;
        }

        // The inode: a move stamps its change and modification times as a
        // write's does, and this put, applied after, keeps them -- the
        // file's contents have not changed. Only the transaction that last
        // touched it moves.
        let mut raw = self.raw_inode(ino)?;
        raw[io::TRANSID..io::TRANSID + 8].copy_from_slice(&generation.to_le_bytes());
        edits.push(ItemEdit::Put(OwnedItem {
            key: DiskKey {
                objectid: ino,
                key_type: INODE_ITEM_KEY,
                offset: 0,
            },
            data: raw,
        }));

        // Every extent given up whole: each run's, unless a reflink
        // shares it.
        let released: Vec<(u64, u64)> = members
            .iter()
            .flatten()
            .filter(|m| !m.shared)
            .map(|m| (m.extent, m.extent + m.extent_len))
            .collect();
        let summed = inode.flags & INODE_NODATASUM == 0;
        let write = DataWrite {
            root: objectid::FS_TREE,
            ino,
            moves,
            adds: Vec::new(),
            time: crate::namespace::now(),
            root_flags: None,
            edits,
            default_subvol: None,
            csum_edits: self.csum_edits(&released, &contents, summed)?,
            releases,
            unrefs,
        };

        // The fs tree leaves the change edits -- the inode's, and each
        // run's items' -- and the checksum tree leaves of each digest item
        // it cuts, removes or adds.
        let dirty: Vec<u64> = {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let mut leaves = std::collections::BTreeSet::new();
            let keys = members.iter().flatten().map(|m| m.key);
            for key in keys.chain(write.edits.iter().map(ItemEdit::key)) {
                leaves.insert(tree.descend(self.fs_tree_root, &key)?.header.bytenr);
            }
            if !write.csum_edits.is_empty() {
                let root = self.tree_root(crate::csum::CSUM_TREE_OBJECTID)?;
                for edit in &write.csum_edits {
                    leaves.insert(tree.descend(root, &edit.key())?.header.bytenr);
                }
            }
            leaves.into_iter().collect()
        };

        let plan = self.plan_transaction_closed_with(&dirty, &write, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, &write, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the defragment's plan does not move the root tree".into())
        })?;

        // `bytes_used` is the sum of every group's `used`: the plan's tree
        // blocks, each copy taken, and each extent given up whole.
        let delta = plan.usage_delta(u64::from(self.sb.nodesize))
            + write
                .moves
                .iter()
                .map(|m| i128::from(m.disk_len))
                .sum::<i128>()
            - released
                .iter()
                .map(|(start, end)| i128::from(end - start))
                .sum::<i128>();
        let bytes_used = match delta {
            0 => None,
            d => Some(
                u64::try_from(i128::from(self.sb.bytes_used) + d).map_err(|_| {
                    Error::UnsupportedFeature(format!(
                        "the superblock's {} bytes used cannot move by {d}",
                        self.sb.bytes_used
                    ))
                })?,
            ),
        };

        // The copies, to every mirror, before any tree block: nothing
        // names these addresses until the superblocks are written.
        for (at, buf) in &contents {
            self.mirror_spans(*at, buf.len())?;
        }
        for (at, buf) in &contents {
            self.write_logical_all_mirrors(*at, buf)?;
        }
        self.commit(
            &blocks,
            &Commit {
                generation,
                root,
                bytes_used,
                invalidate_free_space_tree: false,
                ..Default::default()
            },
        )?;

        *self = self.remount_rw()?;
        Ok(report)
    }

    /// A run's item, checked: its window inside its extent, and its
    /// extent newer than the last snapshot.
    fn member(&self, ino: u64, k: &DiskKey, d: &[u8], last_snapshot: u64) -> Result<Member> {
        let bytenr = le64(d, fe::DISK_BYTENR);
        let offset = le64(d, fe::OFFSET);
        let len = le64(d, fe::NUM_BYTES);
        let (refs, extent_len, generation) = self.extent_item(bytenr)?;
        let inside = bytenr
            .checked_add(offset)
            .is_some_and(|at| window_inside_extent(at, len, bytenr, extent_len));
        if !inside {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: the item at {} references [{bytenr} + {offset}, +{len}), outside \
                 the {extent_len}-byte extent the extent tree records at {bytenr}",
                k.offset
            )));
        }
        if generation <= last_snapshot {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: the extent at {bytenr} is from generation {generation}, at or \
                 before the snapshot taken at {last_snapshot}, so the snapshot may still be \
                 reading it"
            )));
        }
        let ref_offset = k.offset.checked_sub(offset).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "inode {ino}: the item at {} begins {offset} bytes into its extent, before the \
                 start of the file",
                k.offset
            ))
        })?;
        Ok(Member {
            key: *k,
            len,
            extent: bytenr,
            extent_len,
            ref_offset,
            shared: refs != 1,
        })
    }
}

/// Whether an `EXTENT_DATA` item's own fields let it join a run: a
/// regular, unencoded extent, not a hole.
fn movable(d: &[u8]) -> bool {
    d.len() == fe::REGULAR_SIZE
        && d[fe::TYPE] == EXTENT_REGULAR
        && d[fe::COMPRESSION] == 0
        && d[fe::ENCRYPTION] == 0
        && d[fe::OTHER_ENCODING..fe::OTHER_ENCODING + 2] == [0, 0]
        && le64(d, fe::DISK_BYTENR) != 0
        && le64(d, fe::NUM_BYTES) != 0
}

/// The extent an item references, for a regular or preallocated item
/// that is not a hole.
fn extent_of(d: &[u8]) -> Option<u64> {
    if d.len() < fe::REGULAR_SIZE || d[fe::TYPE] == 0 {
        return None;
    }
    Some(le64(d, fe::DISK_BYTENR)).filter(|&b| b != 0)
}

/// An item's `num_bytes`, or for inline data its length.
fn num_bytes(d: &[u8]) -> u64 {
    if d.len() >= fe::REGULAR_SIZE && d[fe::TYPE] != 0 {
        le64(d, fe::NUM_BYTES)
    } else {
        d.len().saturating_sub(fe::INLINE_DATA) as u64
    }
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::{runs, Item};

    const K: u64 = 4096;

    /// Adjacent movable items, `len` sectors each, from offset zero.
    fn file(lens: &[u64]) -> Vec<Item> {
        let mut at = 0;
        lens.iter()
            .map(|&n| {
                let item = Item {
                    start: at,
                    len: n * K,
                    movable: true,
                };
                at += n * K;
                item
            })
            .collect()
    }

    #[test]
    fn adjacent_extents_are_one_run() {
        assert_eq!(runs(&file(&[1, 2, 3]), 1 << 30), vec![0..3]);
    }

    #[test]
    fn a_single_extent_is_left_alone() {
        assert!(runs(&file(&[16]), 1 << 30).is_empty());
        assert!(runs(&[], 1 << 30).is_empty());
    }

    #[test]
    fn an_item_that_cannot_move_ends_the_run_and_stays_out_of_it() {
        let mut items = file(&[1, 1, 1, 1, 1]);
        items[2].movable = false;
        assert_eq!(runs(&items, 1 << 30), vec![0..2, 3..5]);
        items[1].movable = false;
        assert_eq!(
            runs(&items, 1 << 30),
            vec![3..5],
            "a lone movable item between two that cannot move is not a run"
        );
    }

    #[test]
    fn a_hole_between_two_items_ends_the_run() {
        let mut items = file(&[1, 1, 1]);
        items[2].start += K;
        assert_eq!(runs(&items, 1 << 30), vec![0..2]);
    }

    #[test]
    fn a_run_stops_at_the_target_and_the_next_begins_there() {
        // Three sectors to a run at most: [1, 1] then [2, 1], and the
        // last item alone.
        assert_eq!(runs(&file(&[1, 1, 2, 1, 1]), 3 * K), vec![0..2, 2..4]);
    }

    #[test]
    fn an_extent_as_long_as_the_target_is_left_alone() {
        assert_eq!(runs(&file(&[1, 4, 1, 1]), 4 * K), vec![2..4]);
    }
}
