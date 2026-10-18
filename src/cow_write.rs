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
//!    offset zero. A file that asks for compression (`chattr +c`, the
//!    compression property, or a mount set to compress) has its copy
//!    stored compressed, as zlib, LZO or zstd in the kernel's framing for
//!    each and padded to a sector, when that saves a sector (#265); the
//!    old extent may be compressed or not, since the copy is made from
//!    its decoded bytes. A compressed extent decodes to at most one
//!    compression unit (128 KiB), so a longer copy is split into units,
//!    each a new extent of its own and each compressed or not on its own
//!    account, as the kernel stores a long range (#357). The first LZO or
//!    zstd extent on a volume sets that algorithm's incompatible feature
//!    flag, in the same commit.
//! 2. **The file's `EXTENT_DATA` item** is pointed at the copy, or at a
//!    split copy's first unit, in place: the item keeps its size. Each
//!    further unit gets an item of its own, inserted after it, and a leaf
//!    that overflows is split.
//! 3. **The extent tree** gains an `EXTENT_ITEM` for each new extent, with
//!    an inline `EXTENT_DATA_REF` back to the file, and loses the old
//!    extent's — once, when two of the write's items named it. An old
//!    extent with references the write does not replace — a reflink's, or
//!    another item of this file outside the range — keeps its record, less
//!    this item's reference, and stays for them (#261).
//! 4. **The free-space tree** and the **block groups' `used`** follow, and
//!    the superblock's `bytes_used` moves by the copies' length less that
//!    of the extents released.
//! 5. **The checksum tree** loses the released extents' digests — an
//!    `EXTENT_CSUM` item covering one is deleted, or cut down to the
//!    sectors of its neighbours it also covers — and, unless the file is
//!    `nodatasum`, gains one digest per sector of the copy, in the
//!    volume's checksum algorithm (#261). An extent kept for its other
//!    references keeps its digests.
//! 6. **The inode** is stamped with the transaction, a new change count
//!    and new change and modification times. A write into a hole, or past
//!    the end of the file, adds the sectors it allocates to `nbytes`; one
//!    past the end moves `size` to where it stops (#432). One that starts
//!    past the end leaves a gap, an implicit hole with no item; first, as
//!    the kernel does, the old last sector is copied with zeros past the
//!    old end when the write does not cover it (#433). Without the
//!    no-holes feature, explicit hole items keep the ranges before and
//!    after each fill, trimmed or split in the same transaction (#434),
//!    and the gap a write past the end leaves is a hole item of its own,
//!    from the sector after the old end to the one the write starts in,
//!    as the kernel inserts it (#436).
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
//! - a checksum tree leaf the change would empty, other than the tree's
//!   root, or a full one that cannot be split (its parent node has no
//!   room for another pointer): removing a leaf and splitting a node are
//!   not written yet;
//! - a write past the end of a file, on a volume **without the no-holes
//!   feature**, whose gap an extent item past the end (a preallocated one)
//!   already reaches into;
//! - a **preallocated** extent the write covers only part of, or one
//!   another reference shares, or an **inline** one the write does not
//!   lie within, or one that is encrypted or encoded: each changes the
//!   item's kind, not only where it points. A write within inline data
//!   rewrites that item, and the file stays inline (#437); compressed
//!   inline data is decoded, written and encoded again (#438); a write
//!   covering the whole of a preallocated item is written where its
//!   extent lies, and the item becomes regular in place (#535);
//! - an extent a **snapshot** may share — one from at or before the last
//!   snapshot, which the snapshot reads through a tree block it shares
//!   and its record counts once for both;
//! - an extent another reference shares whose reference from this file
//!   is held anywhere but inline in its record: which entry to count
//!   down is not known.
//!
//! A write in a `nodatacow` file goes to [`Filesystem::write_at`], which
//! writes in place and commits nothing.

use std::collections::{BTreeMap, BTreeSet};

use crate::chunk::{objectid, DiskKey};
use crate::compression::Compression;
use crate::error::{Error, Result};
use crate::fs::{Filesystem, EXTENT_DATA_KEY, EXTENT_PREALLOC, EXTENT_REGULAR};
use crate::inode::{Inode, INODE_ITEM_KEY, INODE_NODATACOW, INODE_NODATASUM};
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::transaction::{DataAdd, DataMove, DataWrite, ItemEdit};
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
    /// Whether the extent keeps references the write does not replace:
    /// a reflink's, or another item of this file outside the range
    /// (#261). It then loses this item's reference and stays.
    shared: bool,
    /// A span of a hole, whole sectors of it, with no allocated extent:
    /// its copy is a new extent with an item of its own, and adds its
    /// length to the inode's `nbytes` (#261). Explicit hole items are
    /// trimmed or split around the copy on a volume without NO_HOLES.
    hole: bool,
    /// The length of the inline data this target's copy replaces, which
    /// leaves the inode's `nbytes` as the copy's length is added (#422);
    /// zero for every other target.
    inline: u64,
}

/// One extent of a write's copies: a whole item's copy, or one unit of a
/// copy split into several (#357).
struct Piece {
    /// The index of the item it copies, in the write's targets.
    target: usize,
    /// Where in the file it begins.
    file_offset: u64,
    /// How much of the file it covers.
    len: u64,
    /// The bytes it is stored as.
    bytes: Vec<u8>,
    /// Its compression type, as the item records it.
    compression: u8,
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
        self.write_cow(&inode, offset, data, false)
    }

    /// Write `data` at `offset` in a copy-on-write file whose data is
    /// inline, moving that data into a regular extent at offset 0 that
    /// holds it, the write, and zeros to the end of the sector the write
    /// ends in, as the kernel does when inline data outgrows its item
    /// (#422). The write must reach past the inline data.
    pub(crate) fn write_moving_inline(
        &mut self,
        ino: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<usize> {
        let inode = self.read_inode(ino)?;
        if inode.flags & INODE_NODATACOW != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} is nodatacow and holds inline data, and moving that data into \
                 an extent is only implemented for a copy-on-write file"
            )));
        }
        self.write_cow(&inode, offset, data, true)
    }

    fn write_cow(
        &mut self,
        inode: &Inode,
        offset: u64,
        data: &[u8],
        move_inline: bool,
    ) -> Result<usize> {
        let ino = inode.ino;
        let summed = inode.flags & INODE_NODATASUM == 0;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::UnsupportedFeature("write range overflows".into()))?;
        // Inline data the write lies within, or grows while it still fits
        // inline, is rewritten in its item, and the file stays inline, as
        // the kernel leaves it (#437, #530).
        let grows_inline = offset <= inode.size && inode.size < u64::from(self.sb.sectorsize);
        if !move_inline && (end <= inode.size || grows_inline) {
            if let Some(written) = self.write_inline(inode, offset, data)? {
                return Ok(written);
            }
        }
        // A write over the whole of a preallocated item is written where
        // its extent lies, and the item becomes regular (#535).
        if !move_inline && end <= inode.size {
            if let Some(written) = self.write_prealloc(inode, offset, data, summed)? {
                return Ok(written);
            }
        }
        // A write that grows the file plans the sectors past its last
        // extent as a hole is (#432). One that starts past the end leaves a
        // gap, and the kernel zeroes the rest of the old last sector before
        // it makes one: that sector is copied too, reading as zeros past
        // the old end (#433). A volume without no-holes records that gap
        // as a hole item, from the sector after the old end to the one the
        // write starts in, as the kernel's extend does (#436).
        let mut tail = None;
        let mut gap = None;
        if end > inode.size {
            let sector = u64::from(self.sb.sectorsize);
            if offset > inode.size && !inode.size.is_multiple_of(sector) {
                tail = Some(inode.size);
            }
            if !self.sb.has_no_holes() {
                let from = inode.size.checked_next_multiple_of(sector).ok_or_else(|| {
                    Error::UnsupportedFeature(format!("inode {ino}: size overflows"))
                })?;
                let to = offset - offset % sector;
                if from < to {
                    self.check_gap_is_empty(ino, from)?;
                    gap = Some((from, to));
                }
            }
        }

        // Every refusal before anything is allocated.
        let targets = self.plan_cow_targets(ino, tail, offset, end, move_inline)?;

        // Each copy: the item's bytes as they read now, with the write
        // laid over them. Past the end of the file the last sector reads
        // as zeros, and that is what the copy holds there. Then the bytes
        // it is stored as: compressed, for a file that asks for it, when
        // that saves a sector (#265). A compressed extent decodes to at
        // most one compression unit, so a copy that asks for compression
        // and is longer than that is split into units, each an extent of
        // its own, as the kernel stores a long range (#357).
        let algo = self.compression_for(inode)?;
        let unit = if algo == Compression::None {
            usize::MAX
        } else {
            crate::compression::MAX_UNCOMPRESSED
        };
        let mut pieces: Vec<Piece> = Vec::with_capacity(targets.len());
        for (target, t) in targets.iter().enumerate() {
            let size = usize::try_from(t.len)
                .map_err(|_| Error::UnsupportedFeature(format!("an extent of {} bytes", t.len)))?;
            let mut buf = vec![0u8; size];
            self.read_at(ino, t.start, &mut buf)?;
            // The old last sector a write past the end copies may lie
            // wholly before the write, and then takes none of it (#433).
            let from = offset.max(t.start);
            let to = end.min(t.start + t.len);
            if from < to {
                buf[(from - t.start) as usize..(to - t.start) as usize]
                    .copy_from_slice(&data[(from - offset) as usize..(to - offset) as usize]);
            }
            let mut at = t.start;
            for chunk in buf.chunks(unit) {
                let (bytes, compression) = self.encode_copy(chunk.to_vec(), algo);
                pieces.push(Piece {
                    target,
                    file_offset: at,
                    len: chunk.len() as u64,
                    bytes,
                    compression,
                });
                at += chunk.len() as u64;
            }
        }

        let lens: Vec<u64> = pieces.iter().map(|p| p.bytes.len() as u64).collect();
        let news = self.find_data_extents(&lens)?;
        let compression: Vec<u8> = pieces.iter().map(|p| p.compression).collect();
        // An algorithm the volume has not used before is announced in the
        // superblock that first names an extent of it, as the kernel does:
        // a reader that predates it must refuse the volume, not misread it.
        let incompat = if compression.iter().any(|&c| c != COMPRESS_NONE) {
            incompat_for(algo) & !self.sb.incompat_flags
        } else {
            0
        };
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;

        // The first piece of each item's copy takes the item's place; each
        // piece after it is a new extent with an item of its own (#357).
        let mut moves: Vec<DataMove> = Vec::with_capacity(targets.len());
        let mut adds: Vec<DataAdd> = Vec::new();
        // Remove or trim explicit holes before inserting the new data
        // items: a fill at a hole's start replaces its original key.
        let mut edits = self.cow_hole_edits(ino, &targets, generation)?;
        if let Some((from, to)) = gap {
            edits.push(ItemEdit::Put(OwnedItem {
                key: DiskKey {
                    objectid: ino,
                    key_type: EXTENT_DATA_KEY,
                    offset: from,
                },
                data: regular_extent_item(generation, 0, 0, to - from, COMPRESS_NONE),
            }));
        }
        for (p, &new) in pieces.iter().zip(&news) {
            let t = &targets[p.target];
            let disk_len = p.bytes.len() as u64;
            if p.file_offset == t.start && !t.hole {
                moves.push(DataMove {
                    old: t.extent_start,
                    old_len: t.extent_len,
                    old_ref_offset: t.start - t.extent_offset,
                    new,
                    len: p.len,
                    item_len: t.len,
                    disk_len,
                    compression: p.compression,
                    file_offset: t.start,
                    old_shared: t.shared,
                });
            } else {
                adds.push(DataAdd {
                    new,
                    disk_len,
                    file_offset: p.file_offset,
                });
                edits.push(ItemEdit::Put(OwnedItem {
                    key: DiskKey {
                        objectid: ino,
                        key_type: EXTENT_DATA_KEY,
                        offset: p.file_offset,
                    },
                    data: regular_extent_item(generation, new, disk_len, p.len, p.compression),
                }));
            }
        }
        let contents: Vec<(u64, Vec<u8>)> = news
            .iter()
            .copied()
            .zip(pieces.into_iter().map(|p| p.bytes))
            .collect();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // Each extent given up, once, however many of the write's items
        // named it: not one another reference still reads.
        let mut released: Vec<(u64, u64)> = Vec::new();
        for t in targets.iter().filter(|t| !t.shared && !t.hole) {
            let span = (t.extent_start, t.extent_start + t.extent_len);
            if !released.contains(&span) {
                released.push(span);
            }
        }
        // A hole filled adds what it now covers to the inode's `nbytes`,
        // a write past the end moves its `size` there (#432), and the
        // inode item says so; the times move as for any write. A write
        // inside the last sector past the end adds no sector, and still
        // moves the size.
        let filled: u64 = targets.iter().filter(|t| t.hole).map(|t| t.len).sum();
        if filled > 0 || end > inode.size {
            use crate::inode::offsets as io;
            let mut raw = self.raw_inode(ino)?;
            let moved: u64 = targets.iter().map(|t| t.inline).sum();
            let nbytes = inode
                .nbytes
                .checked_add(filled)
                .and_then(|n| n.checked_sub(moved))
                .ok_or_else(|| {
                    Error::UnsupportedFeature(format!("inode {ino}: nbytes overflows"))
                })?;
            raw[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
            let size = end.max(inode.size);
            raw[io::SIZE..io::SIZE + 8].copy_from_slice(&size.to_le_bytes());
            crate::namespace::touch(
                &mut raw,
                generation,
                (now.as_secs(), now.subsec_nanos()),
                &[io::CTIME, io::MTIME],
            );
            edits.push(ItemEdit::Put(OwnedItem {
                key: DiskKey {
                    objectid: ino,
                    key_type: INODE_ITEM_KEY,
                    offset: 0,
                },
                data: raw,
            }));
        }
        let write = DataWrite {
            root: objectid::FS_TREE,
            ino,
            moves,
            adds,
            time: (now.as_secs(), now.subsec_nanos()),
            root_flags: None,
            edits,
            default_subvol: None,
            csum_edits: self.csum_edits(&released, &contents, summed)?,
            releases: Vec::new(),
            unrefs: Vec::new(),
            ..Default::default()
        };

        // The fs tree leaves the write edits: the inode item's, each
        // extent item's, and the leaf each added item lands in. Everything
        // else the transaction touches follows from these and from the
        // data moves.
        let dirty: Vec<u64> = {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let mut leaves = BTreeSet::new();
            let keys = std::iter::once((INODE_ITEM_KEY, 0))
                .chain(targets.iter().map(|t| (EXTENT_DATA_KEY, t.start)))
                .chain(write.adds.iter().map(|a| (EXTENT_DATA_KEY, a.file_offset)))
                .chain(gap.map(|(from, _)| (EXTENT_DATA_KEY, from)));
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

        let plan = self.plan_transaction_closed_with(&dirty, &write, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, &write, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the write's plan does not move the root tree".into())
        })?;

        // `bytes_used` is the sum of every group's `used`, which the plan
        // moved by each copy's length less each released extent's. An
        // extent another reference keeps is still used.
        let delta = plan.usage_delta(u64::from(self.sb.nodesize))
            + write
                .allocations()
                .map(|(_, disk_len, _)| i128::from(disk_len))
                .sum::<i128>()
            - released
                .iter()
                .map(|&(start, end)| i128::from(end - start))
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
        self.commit_setting_incompat(
            &blocks,
            &Commit {
                generation,
                root,
                bytes_used,
                // Maintained, by `apply_free_space`, so it stays valid.
                invalidate_free_space_tree: false,
                ..Default::default()
            },
            incompat,
        )?;

        // The trees this mount holds are the previous generation's.
        *self = self.remount_rw()?;
        Ok(data.len())
    }

    /// Write `data` at `offset` into the inline extent at the start of
    /// `inode`'s data, when the write lies within it (#437): the item is
    /// rewritten with the new bytes, its size unchanged, and stamped with
    /// the transaction; the inode's times, `transid` and `sequence` move as
    /// for any write. Inline data has no extent, extent record, free space
    /// or checksum, so nothing else changes. `None` when the file holds no
    /// inline item at offset 0.
    ///
    /// A write that starts within inline data holding the whole file and
    /// ends past it grows the item, as long as the grown data still fits
    /// inline, and moves the inode's `size` and `nbytes` with it (#530).
    ///
    /// A compressed inline extent (#438) is decoded, written and stored
    /// inline again: compressed when the file asks for compression and
    /// the encoding is smaller, as the kernel decides, otherwise as it is.
    /// The item's size may change with its encoding.
    ///
    /// An encrypted or otherwise encoded inline extent, an uncompressed one
    /// whose length disagrees with its `ram_bytes`, and a rewrite that
    /// inline data could no longer hold are refused.
    fn write_inline(&mut self, inode: &Inode, offset: u64, data: &[u8]) -> Result<Option<usize>> {
        use crate::fs::file_extent as fe;
        use crate::inode::offsets as io;
        use crate::superblock::le64;
        let ino = inode.ino;
        let Some((key, item)) = self.items_of(ino)?.into_iter().find(|(k, d)| {
            k.key_type == EXTENT_DATA_KEY && k.offset == 0 && d.get(fe::TYPE) == Some(&0)
        }) else {
            return Ok(None);
        };
        let end = offset + data.len() as u64;
        let unencoded = item.len() >= fe::INLINE_DATA
            && item[fe::ENCRYPTION] == 0
            && item[fe::OTHER_ENCODING..fe::OTHER_ENCODING + 2] == [0, 0];
        let compressed = unencoded && item[fe::COMPRESSION] != 0;
        if !unencoded
            || (!compressed
                && item.len() as u64 != fe::INLINE_DATA as u64 + le64(&item, fe::RAM_BYTES))
        {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: the inline extent at 0 is encrypted or encoded, or its length \
                 disagrees with its ram_bytes, and writing one is not implemented"
            )));
        }
        let sector = self.sb.sectorsize as usize;
        let mut plain = if compressed {
            crate::compression::decompress(
                Compression::from_byte(item[fe::COMPRESSION])?,
                &item[fe::INLINE_DATA..],
                le64(&item, fe::RAM_BYTES) as usize,
                sector,
                crate::compression::ShortDecode::Pad,
            )?
        } else {
            item[fe::INLINE_DATA..].to_vec()
        };
        let held = plain.len() as u64;
        // A write past the inline data grows the file inline only when that
        // data is the whole file and the write starts within it (#530).
        if end > held && (held != inode.size || offset > held) {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: the write to [{offset}, {end}) reaches past the {held} bytes of \
                 inline data at 0, which are not the whole file or the write starts past them"
            )));
        }
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now = (now.as_secs(), now.subsec_nanos());

        let length = held.max(end);
        plain.resize(length as usize, 0);
        plain[offset as usize..end as usize].copy_from_slice(data);
        let (bytes, compression) = if compressed {
            self.encode_inline(plain, self.compression_for(inode)?)
        } else {
            (plain, COMPRESS_NONE)
        };
        // The kernel keeps compressed inline data no larger than its
        // default `max_inline`, and plain inline data short of a sector,
        // which `btrfs check` requires; data grown inline stays short of
        // a sector however it is stored (#530).
        let fits = if compression == COMPRESS_NONE {
            bytes.len() < sector && bytes.len() <= MAX_INLINE
        } else {
            bytes.len() <= MAX_INLINE
        } && (end <= held || length < sector as u64);
        if !fits {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: the write to [{offset}, {end}) leaves {} bytes of inline \
                 data, more than an inline extent holds, and moving it out of line is \
                 not implemented",
                bytes.len()
            )));
        }
        let mut rewritten = item;
        rewritten[fe::GENERATION..fe::GENERATION + 8].copy_from_slice(&generation.to_le_bytes());
        rewritten[fe::RAM_BYTES..fe::RAM_BYTES + 8].copy_from_slice(&length.to_le_bytes());
        rewritten.truncate(fe::INLINE_DATA);
        rewritten[fe::COMPRESSION] = compression;
        rewritten.extend_from_slice(&bytes);
        let mut raw = self.raw_inode(ino)?;
        // A grown inline file's `size` moves to the write's end, and its
        // `nbytes`, which counts inline data, by as much as the data grew.
        if length > held {
            let nbytes = (inode.nbytes - held.min(inode.nbytes))
                .checked_add(length)
                .ok_or_else(|| {
                    Error::UnsupportedFeature(format!("inode {ino}: nbytes overflows"))
                })?;
            raw[io::SIZE..io::SIZE + 8].copy_from_slice(&length.to_le_bytes());
            raw[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
        }
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME, io::MTIME]);
        let edits = vec![
            ItemEdit::Put(OwnedItem {
                key,
                data: rewritten,
            }),
            ItemEdit::Put(OwnedItem {
                key: DiskKey {
                    objectid: ino,
                    key_type: INODE_ITEM_KEY,
                    offset: 0,
                },
                data: raw,
            }),
        ];
        self.commit_edits(ino, edits, generation, now)?;
        Ok(Some(data.len()))
    }

    /// Write `data` at `offset` over the whole of a preallocated extent
    /// item, the one starting there and covering exactly the write, when
    /// that item is the extent's only reference (#535). The bytes go where
    /// the extent lies — they were never readable, and nothing reads them
    /// until the commit turns the item regular — and the item, stamped
    /// with the transaction, changes kind in place, as the kernel's
    /// `btrfs_mark_extent_written` does for a write covering the item. No
    /// item is added, so the extent's record, the free space and the
    /// inode's `nbytes` stay; the checksum tree gains the written sectors'
    /// digests, unless the file is `nodatasum`. `None` when no such item
    /// starts at `offset` with the write's length: the planner then
    /// refuses a partial write into one by name.
    fn write_prealloc(
        &mut self,
        inode: &Inode,
        offset: u64,
        data: &[u8],
        summed: bool,
    ) -> Result<Option<usize>> {
        use crate::fs::file_extent as fe;
        use crate::inode::offsets as io;
        use crate::superblock::le64;
        let ino = inode.ino;
        let len = data.len() as u64;
        let Some((key, item)) = self.items_of(ino)?.into_iter().find(|(k, d)| {
            k.key_type == EXTENT_DATA_KEY
                && k.offset == offset
                && d.len() >= fe::REGULAR_SIZE
                && d[fe::TYPE] == EXTENT_PREALLOC
                && le64(d, fe::NUM_BYTES) == len
        }) else {
            return Ok(None);
        };
        if item[fe::COMPRESSION] != 0
            || item[fe::ENCRYPTION] != 0
            || item[fe::OTHER_ENCODING..fe::OTHER_ENCODING + 2] != [0, 0]
        {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: the preallocated extent item at {offset} is compressed, \
                 encrypted or encoded, and writing one is not implemented"
            )));
        }
        let extent_start = le64(&item, fe::DISK_BYTENR);
        let at = extent_start + le64(&item, fe::OFFSET);
        let (refs, extent_len, made) = self.extent_item(extent_start)?;
        if !window_inside_extent(at, len, extent_start, extent_len) {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: offset {offset} maps to [{at}, +{len}), outside the \
                 {extent_len}-byte extent the extent tree records at {extent_start}"
            )));
        }
        let last_snapshot = self.fs_tree_last_snapshot()?;
        if refs != 1 || made <= last_snapshot {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: the preallocated extent at {extent_start} has {refs} references \
                 or predates the snapshot taken at {last_snapshot}, and writing one another \
                 reference may read is not implemented"
            )));
        }
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now = (now.as_secs(), now.subsec_nanos());

        let mut rewritten = item;
        rewritten[fe::GENERATION..fe::GENERATION + 8].copy_from_slice(&generation.to_le_bytes());
        rewritten[fe::TYPE] = EXTENT_REGULAR;
        let mut raw = self.raw_inode(ino)?;
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME, io::MTIME]);
        let edits = vec![
            ItemEdit::Put(OwnedItem {
                key,
                data: rewritten,
            }),
            ItemEdit::Put(OwnedItem {
                key: DiskKey {
                    objectid: ino,
                    key_type: INODE_ITEM_KEY,
                    offset: 0,
                },
                data: raw,
            }),
        ];
        let csum_edits = self.csum_edits(&[], &[(at, data.to_vec())], summed)?;

        // The data, to every mirror, before the commit that makes the
        // item regular: until then the item still reads as zeros.
        self.mirror_spans(at, data.len())?;
        self.write_logical_all_mirrors(at, data)?;
        let write = DataWrite {
            root: objectid::FS_TREE,
            ino,
            time: now,
            edits,
            csum_edits,
            ..Default::default()
        };
        self.commit_write(write, generation)?;
        Ok(Some(data.len()))
    }

    /// Refuse a gap past the end of a file that an extent item already
    /// reaches into — a preallocated extent kept past the end — as the
    /// hole item recording the gap would overlap it (#436).
    fn check_gap_is_empty(&self, ino: u64, from: u64) -> Result<()> {
        use crate::fs::file_extent as fe;
        use crate::superblock::le64;
        for (key, data) in self.items_of(ino)? {
            // Inline data (type 0) only ever lies within the file's size.
            if key.key_type != EXTENT_DATA_KEY
                || data.len() < fe::REGULAR_SIZE
                || data[fe::TYPE] == 0
            {
                continue;
            }
            if key.offset.saturating_add(le64(&data, fe::NUM_BYTES)) > from {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: an extent item past the end reaches into the gap a write \
                     past the end leaves, which a hole item cannot record without overlapping it"
                )));
            }
        }
        Ok(())
    }

    /// Preserve what remains of explicit hole items around the sectors
    /// filled by this write (#434). The read-oriented extent list omits
    /// holes, so their original keys and spans come from the raw items.
    /// A hole has no extent record, checksum or allocation to release.
    fn cow_hole_edits(
        &self,
        ino: u64,
        targets: &[Target],
        generation: u64,
    ) -> Result<Vec<ItemEdit>> {
        use crate::fs::{file_extent as fe, EXTENT_REGULAR};
        use crate::superblock::le64;
        let mut edits = Vec::new();
        if self.sb.has_no_holes() || !targets.iter().any(|t| t.hole) {
            return Ok(edits);
        }
        // plan_cow_targets has already decoded and validated these items.
        for (key, data) in self.items_of(ino)? {
            if key.key_type != EXTENT_DATA_KEY
                || data.len() < fe::REGULAR_SIZE
                || data[fe::TYPE] != EXTENT_REGULAR
                || le64(&data, fe::DISK_BYTENR) != 0
            {
                continue;
            }
            let end = key
                .offset
                .checked_add(le64(&data, fe::NUM_BYTES))
                .ok_or_else(|| Error::BadSuperblock(format!("inode {ino}: hole end overflows")))?;
            // Each target covers a maximal gap between allocated items,
            // so an explicit hole can intersect at most one target.
            let Some(t) = targets
                .iter()
                .find(|t| t.hole && t.start < end && key.offset < t.start + t.len)
            else {
                continue;
            };
            let from = key.offset.max(t.start);
            let to = end.min(t.start + t.len);
            edits.push(ItemEdit::Delete(key));
            for (start, stop) in [(key.offset, from), (to, end)] {
                if start < stop {
                    edits.push(ItemEdit::Put(OwnedItem {
                        key: DiskKey {
                            offset: start,
                            ..key
                        },
                        data: regular_extent_item(generation, 0, 0, stop - start, COMPRESS_NONE),
                    }));
                }
            }
        }
        Ok(edits)
    }

    /// The extent items `[offset, end)` touches, each checked writable.
    ///
    /// Every refusal happens here, while nothing is allocated: a window
    /// outside its extent, or an extent from at or before the last
    /// snapshot, which the snapshot may still be reading through a tree
    /// block it shares — its record counts one reference for both.
    ///
    /// An extent newer than the last snapshot is referenced only by file
    /// items, each counted in its record. One whose references are all
    /// items this write replaces is released; one with references beyond
    /// them — a reflink's, or another item of this file outside the range
    /// — is marked shared, and keeps them (#261).
    fn plan_cow_targets(
        &self,
        ino: u64,
        tail: Option<u64>,
        offset: u64,
        end: u64,
        move_inline: bool,
    ) -> Result<Vec<Target>> {
        let pieces = self.file_extents(ino)?;
        let last_snapshot = self.fs_tree_last_snapshot()?;
        let mut out = Vec::new();
        // Each extent's reference count, from its record.
        let mut refs_of: BTreeMap<u64, u64> = BTreeMap::new();
        let sector = u64::from(self.sb.sectorsize);
        let mut pos = offset;
        // Inline data moved into an extent (#422): the copy starts at the
        // inline item's offset, takes its key, and covers whole sectors to
        // the one the write ends in, as a hole's copy does. Inline data is
        // only ever the file's one item, at offset 0, so nothing follows it.
        if move_inline {
            let Some(inline) = pieces.iter().find(|p| p.inline) else {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino} holds no inline data to move"
                )));
            };
            if inline.start != 0 || pieces.len() != 1 || end <= inline.start + inline.len {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: its inline data at {} is not the file's one item, or the \
                     write does not reach past it",
                    inline.start
                )));
            }
            let stop = end.div_ceil(sector).saturating_mul(sector);
            out.push(Target {
                start: 0,
                len: stop,
                extent_start: 0,
                extent_len: 0,
                extent_offset: 0,
                shared: false,
                hole: true,
                inline: inline.len,
            });
            pos = stop;
        }
        // A write past the end of a file whose end is partway through a
        // sector copies the item holding that sector first, which reads as
        // zeros past the old end (#433). The planning below then starts
        // past it; a hole there needs nothing, as it reads as zeros.
        let mut spans = Vec::with_capacity(2);
        if let Some(size) = tail {
            if pieces
                .iter()
                .any(|p| size >= p.start && size < p.start + p.len)
            {
                spans.push((size, size + 1));
            }
        }
        spans.push((pos, end));
        for (mut pos, end) in spans {
            if let Some(last) = out.last() {
                pos = pos.max(last.start + last.len);
            }
            while pos < end {
                let Some(piece) = pieces
                    .iter()
                    .find(|p| pos >= p.start && pos < p.start + p.len)
                else {
                    // A hole: whole sectors of it, from the sector the write
                    // starts in to the one it ends in, never past the next
                    // item. The sectors read as zeros, and the copy keeps them.
                    let start = (pos / sector * sector).max(
                        pieces
                            .iter()
                            .map(|p| p.start + p.len)
                            .filter(|&e| e <= pos)
                            .max()
                            .unwrap_or(0),
                    );
                    let next = pieces
                        .iter()
                        .map(|p| p.start)
                        .filter(|&s| s > pos)
                        .min()
                        .unwrap_or(u64::MAX);
                    let stop = end.div_ceil(sector).saturating_mul(sector).min(next);
                    out.push(Target {
                        start,
                        len: stop - start,
                        extent_start: 0,
                        extent_len: 0,
                        extent_offset: 0,
                        shared: false,
                        hole: true,
                        inline: 0,
                    });
                    pos = stop;
                    continue;
                };
                // A compressed extent's window is in its decoded bytes, which
                // reading it already bounds by `ram_bytes`; on disk the whole
                // extent is released, so there is no window to check there.
                let logical = if piece.compressed {
                    None
                } else {
                    Some(piece.logical.ok_or_else(|| {
                        Error::UnsupportedFeature(format!(
                            "inode {ino}: offset {pos} is inline or preallocated, and writing it \
                         changes the item's kind, which is not implemented"
                        ))
                    })?)
                };
                let (refs, extent_len, generation) = self.extent_item(piece.extent_start)?;
                if let Some(logical) = logical {
                    if !window_inside_extent(logical, piece.len, piece.extent_start, extent_len) {
                        return Err(Error::UnsupportedFeature(format!(
                            "inode {ino}: offset {pos} maps to [{logical}, +{}), outside the \
                         {extent_len}-byte extent the extent tree records at {}",
                            piece.len, piece.extent_start
                        )));
                    }
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
                    extent_offset: piece.ref_offset,
                    shared: false,
                    hole: false,
                    inline: 0,
                });
                refs_of.insert(piece.extent_start, refs);
                pos = piece.start + piece.len;
            }
        }
        // Shared when the extent has references beyond the write's own
        // items. More items than its record counts is a record that does
        // not describe this file, and nothing is written over it.
        let mut ours: BTreeMap<u64, u64> = BTreeMap::new();
        for t in out.iter().filter(|t| !t.hole) {
            *ours.entry(t.extent_start).or_default() += 1;
        }
        for t in out.iter_mut().filter(|t| !t.hole) {
            let (items, refs) = (ours[&t.extent_start], refs_of[&t.extent_start]);
            if items > refs {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}: {items} of the file's items name the extent at {}, and its \
                     record counts {refs} references",
                    t.extent_start
                )));
            }
            t.shared = items < refs;
        }
        Ok(out)
    }

    /// How copies of `inode` are stored (#265), decided as the kernel
    /// decides it:
    ///
    /// - a `nodatacow` or `nodatasum` file is never compressed: a
    ///   compressed extent is always copied on write and always
    ///   checksummed;
    /// - the compression property (`btrfs.compression`), when it names an
    ///   algorithm, chooses it, and `no` or `none` declines compression;
    ///   the property overrides both inode flags, which is what the kernel
    ///   does with it when it loads the inode;
    /// - otherwise `NOCOMPRESS` declines it, and the `COMPRESS` flag
    ///   (`chattr +c`) asks for it in the mount's algorithm, zlib when the
    ///   mount names none;
    /// - otherwise the mount decides: [`Filesystem::set_compression`] is
    ///   the kernel's `compress=` mount option.
    ///
    /// Compression is the file's preference, not part of its contents: a
    /// copy stored uncompressed is read the same by every reader.
    fn compression_for(&self, inode: &Inode) -> Result<Compression> {
        if inode.flags & (INODE_NODATACOW | INODE_NODATASUM) != 0 {
            return Ok(Compression::None);
        }
        if let Some(value) = self.get_xattr(inode.ino, COMPRESSION_PROPERTY)? {
            if let Some(chosen) = Compression::from_property(&value) {
                return Ok(chosen);
            }
        }
        if inode.flags & INODE_NOCOMPRESS != 0 {
            return Ok(Compression::None);
        }
        let mount = self.compression();
        if inode.flags & INODE_COMPRESS != 0 && mount == Compression::None {
            return Ok(Compression::Zlib);
        }
        Ok(mount)
    }

    /// The bytes a copy is stored as, and its compression type: `algo`'s
    /// encoding padded to a sector when that saves at least a sector, as
    /// the kernel requires before it keeps a compressed extent; otherwise
    /// the bytes as they are. A copy longer than a compressed extent may
    /// hold (128 KiB) is never compressed; the write splits a copy that
    /// asks for compression into units before it gets here (#357).
    fn encode_copy(&self, plain: Vec<u8>, algo: Compression) -> (Vec<u8>, u8) {
        let sector = self.sb.sectorsize as usize;
        if plain.len() > crate::compression::MAX_UNCOMPRESSED {
            return (plain, COMPRESS_NONE);
        }
        let Some(mut packed) = crate::compression::compress(algo, &plain, sector) else {
            return (plain, COMPRESS_NONE);
        };
        let on_disk = packed.len().next_multiple_of(sector);
        if on_disk + sector > plain.len() {
            return (plain, COMPRESS_NONE);
        }
        packed.resize(on_disk, 0);
        (packed, algo.to_byte())
    }

    /// The bytes inline data is stored as, and its compression type:
    /// `algo`'s encoding when it is smaller than the data, as the kernel
    /// keeps compressed inline data; otherwise the data as it is (#438).
    fn encode_inline(&self, plain: Vec<u8>, algo: Compression) -> (Vec<u8>, u8) {
        match crate::compression::compress(algo, &plain, self.sb.sectorsize as usize) {
            Some(packed) if packed.len() < plain.len() => (packed, algo.to_byte()),
            _ => (plain, COMPRESS_NONE),
        }
    }

    /// The checksum tree edits a write makes (#261), or a truncate
    /// (#262): `released` holds each extent given up, as `[start, end)`.
    ///
    /// Every `EXTENT_CSUM` item covering a sector of a released extent is
    /// deleted, and what it also covered outside those
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
    pub(crate) fn csum_edits(
        &self,
        released: &[(u64, u64)],
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
        // Each item covering a released sector, whole.
        let mut items: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        {
            let reader = self.pool_reader();
            let tree = reader.tree();
            for &(start, end) in released {
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
            let runs = kept_runs(*offset, data, size, sector, released);
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

    /// The checksum tree edits that carry each extent in `moved`, given as
    /// `(old, new, length)`, from its old address to its new one (#493):
    /// its digests are taken from the old sectors exactly as they are and
    /// filed under the new, after the deletes [`Filesystem::csum_edits`]
    /// makes for an extent it releases. A sector with no digest gets none.
    pub(crate) fn csum_move_edits(&self, moved: &[(u64, u64, u64)]) -> Result<Vec<ItemEdit>> {
        use crate::csum::{CSUM_TREE_OBJECTID, EXTENT_CSUM_KEY, EXTENT_CSUM_OBJECTID};
        let size = self.sb.csum_type.digest_len();
        let sector = u64::from(self.sb.sectorsize);
        let Ok(root) = self.tree_root(CSUM_TREE_OBJECTID) else {
            return Ok(Vec::new());
        };
        let released: Vec<(u64, u64)> = moved
            .iter()
            .map(|&(old, _, len)| (old, old + len))
            .collect();
        let mut edits = self.csum_edits(&released, &[], false)?;

        // Every digest, by the sector it describes once moved.
        let mut digests: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        {
            let reader = self.pool_reader();
            let tree = reader.tree();
            for &(_, new, len) in moved {
                let stale = crate::csum::digests_for_range(&tree, root, size, sector, new, len)?;
                if !stale.is_empty() {
                    return Err(Error::UnsupportedFeature(format!(
                        "the checksum tree already holds {} digests for the space at {new} \
                         the relocation was given, which nothing allocates",
                        stale.len()
                    )));
                }
            }
        }
        for &(old, new, len) in moved {
            for (at, digest) in self.data_digests(old, len)? {
                digests.insert(new + (at - old), digest);
            }
        }
        // Runs of adjacent sectors, each cut at the kernel's cap on one
        // item, MAX_CSUM_ITEMS.
        let per_item = ((self.sb.nodesize as usize - LEAF_HEADER - 2 * ITEM_HEADER) / size)
            .saturating_sub(1)
            .max(1);
        let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
        for (at, digest) in digests {
            match runs.last_mut() {
                Some((start, packed))
                    if packed.len() / size < per_item
                        && *start + (packed.len() / size) as u64 * sector == at =>
                {
                    packed.extend_from_slice(&digest[..size]);
                }
                _ => runs.push((at, digest[..size].to_vec())),
            }
        }
        edits.extend(runs.into_iter().map(|(at, data)| {
            ItemEdit::Put(OwnedItem {
                key: DiskKey {
                    objectid: EXTENT_CSUM_OBJECTID,
                    key_type: EXTENT_CSUM_KEY,
                    offset: at,
                },
                data,
            })
        }));
        Ok(edits)
    }
}

/// `BTRFS_INODE_NOCOMPRESS` and `BTRFS_INODE_COMPRESS`.
const INODE_NOCOMPRESS: u64 = 1 << 3;
const INODE_COMPRESS: u64 = 1 << 11;

/// An extent item's `compression` byte for a copy stored as it is.
const COMPRESS_NONE: u8 = 0;

/// The most inline data the kernel keeps in an extent item: its default
/// `max_inline` mount option.
const MAX_INLINE: usize = 2048;

/// The attribute the kernel keeps the compression property in.
const COMPRESSION_PROPERTY: &[u8] = b"btrfs.compression";

/// A regular `EXTENT_DATA` item referencing the whole of the `len` bytes
/// stored as `disk_len` at `bytenr`, written in transaction `generation`
/// (#357).
fn regular_extent_item(
    generation: u64,
    bytenr: u64,
    disk_len: u64,
    len: u64,
    compression: u8,
) -> Vec<u8> {
    use crate::fs::file_extent as fe;
    let mut d = vec![0u8; fe::REGULAR_SIZE];
    let mut put = |at: usize, v: u64| d[at..at + 8].copy_from_slice(&v.to_le_bytes());
    put(fe::GENERATION, generation);
    put(fe::RAM_BYTES, len);
    put(fe::DISK_BYTENR, bytenr);
    put(fe::DISK_NUM_BYTES, disk_len);
    put(fe::OFFSET, 0);
    put(fe::NUM_BYTES, len);
    d[fe::COMPRESSION] = compression;
    d[fe::TYPE] = crate::fs::EXTENT_REGULAR;
    d
}

/// The incompatible feature flag an extent compressed with `algo` needs:
/// LZO's and zstd's have one each, and zlib, which every kernel with
/// Btrfs reads, has none.
fn incompat_for(algo: Compression) -> u64 {
    use crate::superblock::incompat;
    match algo {
        Compression::Lzo => incompat::COMPRESS_LZO,
        Compression::Zstd => incompat::COMPRESS_ZSTD,
        Compression::None | Compression::Zlib => 0,
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
