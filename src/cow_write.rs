//! Writing an ordinary, copy-on-write file (#61).
//!
//! [`crate::write`] overwrites a `nodatacow` file where its bytes lie.
//! Every other file is copy-on-write: an extent, once committed, is never
//! changed, so writing a byte of one means a new extent holding the new
//! bytes, the file's item pointed at it, and the old one released — then
//! all of that committed as one transaction, so a crash leaves either the
//! old file or the new one and never a mixture.
//!
//! # What a write does
//!
//! For each extent item the range touches:
//!
//! 1. **A new extent** is found in a data block group
//!    (`Filesystem::find_data_extents`), as long as the bytes the item
//!    covers, and filled with them — the old contents with the write
//!    applied. The copy is the item's window of the old extent, not the
//!    whole of it, so the new item references all of its extent from
//!    offset zero.
//! 2. **The file's `EXTENT_DATA` item** is pointed at it, in place: the
//!    item keeps its size, so its leaf cannot overflow.
//! 3. **The extent tree** loses the old extent's `EXTENT_ITEM` and gains
//!    one for the new, with an inline `EXTENT_DATA_REF` back to the file.
//! 4. **The free-space tree** and the **block groups' `used`** follow, and
//!    the superblock's `bytes_used` moves by whatever the copy is shorter
//!    than the extent it replaced.
//! 5. **The checksum tree** loses the old extent's digests — an
//!    `EXTENT_CSUM` item covering it is deleted, or cut down to the
//!    sectors of its neighbours it also covers — and, unless the file is
//!    `nodatasum`, gains one digest per sector of the copy, in the
//!    volume's checksum algorithm (#261).
//! 6. **The inode** is stamped with the transaction, a new change count
//!    and new change and modification times.
//!
//! The data goes to every mirror first; then the tree blocks, a flush,
//! the superblocks and a flush, in [`Filesystem::commit`]'s order. Until
//! the superblocks land, nothing points at the new extent.
//!
//! # What it refuses
//!
//! Each refusal is a case not yet written, named so a caller can tell
//! which it met:
//!
//! - a checksum tree leaf with no room for the copy's digests, or one the
//!   change would empty: splitting and removing a leaf for an item edit
//!   are not written yet;
//! - a write that **grows** the file, or lands in a **hole**: both
//!   allocate where the file has no extent item to repoint;
//! - an **inline**, **preallocated** or **compressed** extent: each
//!   changes the item's kind, not only where it points;
//! - a **shared** extent — more than one reference, or one the last
//!   snapshot could still be reading: releasing it would free bytes
//!   another reader holds.
//!
//! A write in a `nodatacow` file goes to [`Filesystem::write_at`], which
//! writes in place and commits nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::chunk::{objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::{Filesystem, EXTENT_DATA_KEY};
use crate::inode::{Inode, INODE_ITEM_KEY, INODE_NODATACOW, INODE_NODATASUM};
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::transaction::{DataMove, DataWrite, ItemEdit};
use crate::write::window_inside_extent;

/// How many rounds a write's plan has to close over its own bookkeeping.
/// Each round adds the extent and free-space leaves the last one's moves
/// dirtied; a write that has not settled in this many is refused.
const PLAN_ROUNDS: usize = 64;

/// One extent item the write replaces, as the planner found it.
#[derive(Debug, Clone, Copy)]
struct Target {
    /// The item's key offset: where in the file it begins.
    start: u64,
    /// How much of the file it covers, its `num_bytes`.
    len: u64,
    /// The extent it references.
    extent_start: u64,
    /// That extent's length, from the extent tree.
    extent_len: u64,
    /// Where in that extent the item's window begins.
    extent_offset: u64,
}

impl Filesystem {
    /// Write `data` at `offset` in a regular file, and commit it.
    ///
    /// A `nodatacow` file is written in place, exactly as
    /// [`Filesystem::write_at`] does. Any other file is written
    /// copy-on-write, as one committed transaction per call — see the
    /// [module documentation](crate::cow_write) for what that changes and what it
    /// still refuses. Nothing is written unless the whole range can be.
    ///
    /// Takes `&mut self` because a commit moves the trees this mount
    /// reads: once it lands, the mount is reopened on the new
    /// generation, so the next read or write starts from what was just
    /// committed rather than from the superseded roots.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but a regular file, and
    /// [`Error::UnsupportedFeature`] naming the case for every refusal.
    /// If reopening the mount after the commit fails, the write is
    /// committed and the error is returned; this handle then still reads
    /// the previous generation and must be dropped.
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() {
            return Err(Error::NotAFile);
        }
        if inode.flags & INODE_NODATACOW != 0 {
            return self.write_at(ino, offset, data);
        }
        self.write_cow(&inode, offset, data)
    }

    fn write_cow(&mut self, inode: &Inode, offset: u64, data: &[u8]) -> Result<usize> {
        let ino = inode.ino;
        let summed = inode.flags & INODE_NODATASUM == 0;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::UnsupportedFeature("write range overflows".into()))?;
        if end > inode.size {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: writing to {end} would grow the file past its {} bytes, which \
                 adds an extent rather than replacing one",
                inode.size
            )));
        }

        // Every refusal before anything is allocated.
        let targets = self.plan_cow_targets(ino, offset, end)?;

        let lens: Vec<u64> = targets.iter().map(|t| t.len).collect();
        let news = self.find_data_extents(&lens)?;

        // Each copy: the item's bytes as they read now, with the write
        // laid over them. Past the end of the file the last sector reads
        // as zeros, and that is what the copy holds there.
        let mut contents: Vec<(u64, Vec<u8>)> = Vec::with_capacity(targets.len());
        for (t, &at) in targets.iter().zip(&news) {
            let size = usize::try_from(t.len)
                .map_err(|_| Error::UnsupportedFeature(format!("an extent of {} bytes", t.len)))?;
            let mut buf = vec![0u8; size];
            self.read_at(ino, t.start, &mut buf)?;
            let from = offset.max(t.start);
            let to = end.min(t.start + t.len);
            buf[(from - t.start) as usize..(to - t.start) as usize]
                .copy_from_slice(&data[(from - offset) as usize..(to - offset) as usize]);
            contents.push((at, buf));
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let write = DataWrite {
            root: objectid::FS_TREE,
            ino,
            moves: targets
                .iter()
                .zip(&news)
                .map(|(t, &new)| DataMove {
                    old: t.extent_start,
                    old_len: t.extent_len,
                    old_ref_offset: t.start - t.extent_offset,
                    new,
                    len: t.len,
                    file_offset: t.start,
                })
                .collect(),
            time: (now.as_secs(), now.subsec_nanos()),
            root_flags: None,
            edits: Vec::new(),
            csum_edits: self.csum_edits(&targets, &contents, summed)?,
        };

        // The fs tree leaves the write edits: the inode item's, and each
        // extent item's. Everything else the transaction touches follows
        // from these and from the data moves.
        let dirty: Vec<u64> = {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let mut leaves = BTreeSet::new();
            let keys = std::iter::once((INODE_ITEM_KEY, 0))
                .chain(targets.iter().map(|t| (EXTENT_DATA_KEY, t.start)));
            for (key_type, key_offset) in keys {
                let key = DiskKey {
                    objectid: ino,
                    key_type,
                    offset: key_offset,
                };
                leaves.insert(tree.descend(self.fs_tree_root, &key)?.header.bytenr);
            }
            // And the checksum tree's, for each digest item the write
            // cuts, removes or adds.
            if !write.csum_edits.is_empty() {
                let root = self.tree_root(crate::csum::CSUM_TREE_OBJECTID)?;
                for edit in &write.csum_edits {
                    leaves.insert(tree.descend(root, &edit.key())?.header.bytenr);
                }
            }
            leaves.into_iter().collect()
        };

        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let plan = self.plan_transaction_closed_with(&dirty, &write, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, &write, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the write's plan does not move the root tree".into())
        })?;

        // `bytes_used` is the sum of every group's `used`, which the plan
        // moved by each copy's length less its extent's.
        let delta = plan.usage_delta(u64::from(self.sb.nodesize))
            + write
                .moves
                .iter()
                .map(|m| i128::from(m.len) - i128::from(m.old_len))
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

        // The data, to every mirror, before any tree block: nothing
        // names these addresses until the superblocks are written, and
        // the commit's first flush puts them on the device before that.
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
                // Maintained, by `apply_free_space`, so it stays valid.
                invalidate_free_space_tree: false,
                ..Default::default()
            },
        )?;

        // The trees this mount holds are the previous generation's.
        *self = self.remount_rw()?;
        Ok(data.len())
    }

    /// The extent items `[offset, end)` touches, each checked writable.
    ///
    /// Every refusal happens here, while nothing is allocated. The checks
    /// are [`Filesystem::write_at`]'s, for the same reasons: a window
    /// outside its extent, more than one reference, or an extent from at
    /// or before the last snapshot.
    fn plan_cow_targets(&self, ino: u64, offset: u64, end: u64) -> Result<Vec<Target>> {
        let pieces = self.file_extents(ino)?;
        let last_snapshot = self.fs_tree_last_snapshot()?;
        let mut out = Vec::new();
        let mut pos = offset;
        while pos < end {
            let Some(piece) = pieces
                .iter()
                .find(|p| pos >= p.start && pos < p.start + p.len)
            else {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: offset {pos} is a hole, and filling it adds an extent \
                     rather than replacing one"
                )));
            };
            if piece.compressed {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: offset {pos} is in a compressed extent, and replacing one \
                     is not implemented"
                )));
            }
            let Some(logical) = piece.logical else {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: offset {pos} is inline or preallocated, and writing it \
                     changes the item's kind, which is not implemented"
                )));
            };
            let (refs, extent_len, generation) = self.extent_item(piece.extent_start)?;
            if !window_inside_extent(logical, piece.len, piece.extent_start, extent_len) {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: offset {pos} maps to [{logical}, +{}), outside the \
                     {extent_len}-byte extent the extent tree records at {}",
                    piece.len, piece.extent_start
                )));
            }
            if refs != 1 {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: the extent at {} has {refs} references, so a snapshot or \
                     another file still reads it, and releasing one reference of several \
                     is not implemented",
                    piece.extent_start
                )));
            }
            if generation <= last_snapshot {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: the extent at {} is from generation {generation}, at or \
                     before the snapshot taken at {last_snapshot}, so the snapshot may still \
                     be reading it",
                    piece.extent_start
                )));
            }
            out.push(Target {
                start: piece.start,
                len: piece.len,
                extent_start: piece.extent_start,
                extent_len,
                extent_offset: logical - piece.extent_start,
            });
            pos = piece.start + piece.len;
        }
        Ok(out)
    }

    /// The checksum tree edits a write makes (#261).
    ///
    /// Every `EXTENT_CSUM` item covering a sector of an extent the write
    /// releases is deleted, and what it also covered outside those
    /// extents — a neighbour's digests, which the kernel packs into the
    /// same item when extents are contiguous — is put back as items of
    /// its own. Then, for a checksummed file, each copy gains a digest per
    /// sector of exactly the bytes written to it, in items no larger than
    /// the kernel's.
    ///
    /// The copy's space must have no digests already: one there would be
    /// a leftover the new digests would overlap, and that is refused.
    /// Deletes come before puts, so a leaf is at its smallest before
    /// anything is added to it.
    fn csum_edits(
        &self,
        targets: &[Target],
        contents: &[(u64, Vec<u8>)],
        summed: bool,
    ) -> Result<Vec<ItemEdit>> {
        use crate::csum::{CSUM_TREE_OBJECTID, EXTENT_CSUM_KEY, EXTENT_CSUM_OBJECTID};
        let csum = self.sb.csum_type;
        let size = csum.digest_len();
        let sector = u64::from(self.sb.sectorsize);
        let root = match self.tree_root(CSUM_TREE_OBJECTID) {
            Ok(root) => root,
            Err(_) if !summed => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let key = |offset: u64| DiskKey {
            objectid: EXTENT_CSUM_OBJECTID,
            key_type: EXTENT_CSUM_KEY,
            offset,
        };
        let released: Vec<(u64, u64)> = targets
            .iter()
            .map(|t| (t.extent_start, t.extent_start + t.extent_len))
            .collect();

        // Each item covering a released sector, whole.
        let mut items: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        {
            let reader = self.pool_reader();
            let tree = reader.tree();
            for &(start, end) in &released {
                let found =
                    crate::csum::digests_for_range(&tree, root, size, sector, start, end - start)?;
                let Some(&first) = found.keys().next() else {
                    continue;
                };
                // The item holding the first sector begins at or before
                // it; walk from there over every item the range reaches.
                let leaf = tree.descend(root, &key(first))?;
                let from = leaf
                    .body
                    .items()
                    .unwrap_or(&[])
                    .iter()
                    .rev()
                    .find(|i| {
                        i.key.objectid == EXTENT_CSUM_OBJECTID
                            && i.key.key_type == EXTENT_CSUM_KEY
                            && i.key.offset <= first
                    })
                    .map_or(key(first), |i| i.key);
                tree.for_each_from(root, &from, &mut |k: &DiskKey, data: &[u8]| {
                    if k.objectid != EXTENT_CSUM_OBJECTID
                        || k.key_type != EXTENT_CSUM_KEY
                        || k.offset >= end
                    {
                        return Ok(false);
                    }
                    let covers_end = k.offset + (data.len() / size) as u64 * sector;
                    if covers_end > start {
                        items.insert(k.offset, data.to_vec());
                    }
                    Ok(true)
                })?;
            }
        }

        let mut deletes = Vec::new();
        let mut puts = Vec::new();
        for (offset, data) in &items {
            let runs = kept_runs(*offset, data, size, sector, &released);
            if !runs.iter().any(|(at, _)| at == offset) {
                deletes.push(ItemEdit::Delete(key(*offset)));
            }
            for (at, bytes) in runs {
                puts.push(ItemEdit::Put(OwnedItem {
                    key: key(at),
                    data: bytes,
                }));
            }
        }

        if summed {
            // The kernel's own cap on one item, MAX_CSUM_ITEMS: what fits
            // in a leaf beside two item headers, less one.
            let per_item = ((self.sb.nodesize as usize - LEAF_HEADER - 2 * ITEM_HEADER) / size)
                .saturating_sub(1)
                .max(1);
            let reader = self.pool_reader();
            let tree = reader.tree();
            for (at, bytes) in contents {
                let len = bytes.len() as u64;
                let stale = crate::csum::digests_for_range(&tree, root, size, sector, *at, len)?;
                if !stale.is_empty() {
                    return Err(Error::UnsupportedFeature(format!(
                        "the checksum tree already holds {} digests for the space at {at} the \
                         copy was given, which nothing allocates",
                        stale.len()
                    )));
                }
                let sectors: Vec<&[u8]> = bytes.chunks(sector as usize).collect();
                for (n, group) in sectors.chunks(per_item).enumerate() {
                    let mut packed = Vec::with_capacity(group.len() * size);
                    for s in group {
                        packed.extend_from_slice(&csum.digest(s)[..size]);
                    }
                    puts.push(ItemEdit::Put(OwnedItem {
                        key: key(at + (n * per_item) as u64 * sector),
                        data: packed,
                    }));
                }
            }
        }
        deletes.extend(puts);
        Ok(deletes)
    }
}

/// A leaf's header, and the header of each item in it.
const LEAF_HEADER: usize = 101;
const ITEM_HEADER: usize = 25;

/// The runs of an `EXTENT_CSUM` item's sectors that no released extent
/// holds, each with its digests: what is kept of the item when the
/// extents in `released` (each `[start, end)`) give theirs up. The item
/// begins at `offset` and holds `size`-byte digests, one per `sector`.
fn kept_runs(
    offset: u64,
    data: &[u8],
    size: usize,
    sector: u64,
    released: &[(u64, u64)],
) -> Vec<(u64, Vec<u8>)> {
    let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
    for (i, digest) in data.chunks_exact(size).enumerate() {
        let at = offset + i as u64 * sector;
        if released.iter().any(|&(a, b)| at >= a && at < b) {
            continue;
        }
        match runs.last_mut() {
            Some((run, bytes)) if *run + (bytes.len() / size) as u64 * sector == at => {
                bytes.extend_from_slice(digest)
            }
            _ => runs.push((at, digest.to_vec())),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::kept_runs;

    /// Eight 4-byte digests from 0x10000, one per 4 KiB sector, each
    /// holding its own index.
    fn item() -> Vec<u8> {
        (0u32..8).flat_map(|i| i.to_le_bytes()).collect()
    }

    fn digests(range: std::ops::Range<u32>) -> Vec<u8> {
        range.flat_map(|i| i.to_le_bytes()).collect()
    }

    #[test]
    fn an_item_keeps_the_sectors_on_either_side_of_a_released_extent() {
        let runs = kept_runs(0x10000, &item(), 4, 4096, &[(0x11000, 0x13000)]);
        assert_eq!(
            runs,
            vec![(0x10000, digests(0..1)), (0x13000, digests(3..8))],
            "the head stays under its key and the tail moves to the extent's end"
        );
    }

    #[test]
    fn an_item_wholly_inside_released_extents_keeps_nothing() {
        assert!(kept_runs(0x10000, &item(), 4, 4096, &[(0x10000, 0x18000)]).is_empty());
        assert!(
            kept_runs(
                0x10000,
                &item(),
                4,
                4096,
                &[(0x10000, 0x14000), (0x14000, 0x18000)]
            )
            .is_empty(),
            "two adjacent released extents release the whole item between them"
        );
    }

    #[test]
    fn an_item_losing_only_its_head_keeps_its_tail_under_a_new_key() {
        let runs = kept_runs(0x10000, &item(), 4, 4096, &[(0xf000, 0x12000)]);
        assert_eq!(runs, vec![(0x12000, digests(2..8))]);
    }

    #[test]
    fn an_item_no_released_extent_reaches_is_kept_whole() {
        let runs = kept_runs(0x10000, &item(), 4, 4096, &[(0x20000, 0x30000)]);
        assert_eq!(runs, vec![(0x10000, item())]);
    }
}
