//! Cloning a range of one file into another: reflink and clone-range
//! (#269).
//!
//! A clone copies no data. The destination gains `EXTENT_DATA` items
//! naming the source's extents, each windowed to the part of it the range
//! covers, and each of those extents gains a reference from the item that
//! now names it: its `EXTENT_ITEM`'s count goes up by one and it carries an
//! inline `EXTENT_DATA_REF` for the destination's inode, at the file
//! offset the extent's first byte would sit at. Nothing is allocated, so
//! the free-space tree, the block groups' `used` and the checksum tree --
//! keyed by disk address, and so already covering the shared bytes -- do
//! not change. The destination's size grows to cover what it gains, its
//! `nbytes` is counted again from its items, and its change and
//! modification times move, as the kernel's clone moves them. It is one
//! committed transaction on the top-level subvolume, through the same
//! planner and commit as [`crate::truncate`].
//!
//! What the destination held in the range is replaced, as the kernel's
//! clone replaces it: an item wholly inside the range is deleted, and its
//! extent released when the deleted items were all its references, or
//! the items' references dropped from it when anything else still holds
//! one; an item that runs into the range is cut short at its start, one
//! that runs out of it starts again at its end, and one that runs through
//! it is split in two, the second half adding a reference to the extent
//! both halves name.
//!
//! The kernel's rules for the range hold here too: both offsets are whole
//! sectors, and so is the length unless it runs to the source's end, in
//! which case the source's last, partial sector is cloned whole and must
//! not land inside the destination. A length of zero clones to the
//! source's end, as `FICLONERANGE` does. A file may be cloned into itself
//! where the two ranges do not overlap.
//!
//! A write into either copy afterwards copies the extent it lands in and
//! drops that copy's reference (#261); truncating or unlinking one drops
//! its references and leaves the extents to the other (#262).
//!
//! # What it refuses
//!
//! - a destination with inline data: replacing or keeping it beside the
//!   clone means moving that data into an extent, which a clone does not
//!   do yet;
//! - inline data in the source range, and encrypted or otherwise encoded
//!   extents;
//! - an extent the clone adds a reference to whose references are kept as
//!   items of their own, or would be once this one is added -- the kernel
//!   files them so past about 33 references at a 16 KiB node -- since
//!   filing a reference that way is not written;
//! - a volume without the no-holes feature, where a hole in the range
//!   would have to be recorded as an item;
//! - growing a destination whose last sector is partly past its end, as
//!   [`Filesystem::truncate`] refuses it;
//! - one file that checksums its data and one that does not, which the
//!   kernel refuses so that no file is left partly checksummed;
//! - any subvolume other than the top level.
//!
//! A range that breaks the kernel's rules is refused the same way as what
//! is not written yet, with [`Error::UnsupportedFeature`] naming the rule.

use std::collections::BTreeMap;

use crate::backref::{EXTENT_DATA_REF, SHARED_DATA_REF};
use crate::chunk::{objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::{file_extent as fe, Filesystem, EXTENT_DATA_KEY};
use crate::inode::{offsets as io, Inode, INODE_ITEM_KEY, INODE_NODATASUM};
use crate::leaf_edit::OwnedItem;
use crate::transaction::{DataRelease, DataShare, DataWrite, ItemEdit};
use crate::write::window_inside_extent;

/// `BTRFS_FILE_EXTENT_INLINE`.
const INLINE: u8 = 0;

impl Filesystem {
    /// Clone `len` bytes of regular file `src`, from `src_offset`, into
    /// regular file `dst` at `dst_offset`, as one committed transaction:
    /// the two then share those extents, and whatever `dst` held in the
    /// range is replaced. A `len` of zero clones to the end of `src`.
    /// `src` and `dst` may be the same file when the two ranges do not
    /// overlap.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but two regular files, and
    /// [`Error::UnsupportedFeature`] for a range the kernel's rules refuse
    /// and for every refusal the [module documentation](crate::clone)
    /// lists. Nothing is written unless everything is.
    pub fn clone_range(
        &mut self,
        src: u64,
        src_offset: u64,
        dst: u64,
        dst_offset: u64,
        len: u64,
    ) -> Result<()> {
        let (source, target) = self.remap_inodes(src, dst)?;
        let range = self.clone_span(src, &source, src_offset, dst, &target, dst_offset, len)?;
        let Some(span) = range else {
            return Ok(());
        };
        self.remap(src, src_offset, dst, &target, dst_offset, span)
    }

    /// The two inodes a clone joins, once both are regular
    /// files on a writable top-level subvolume that may be joined.
    fn remap_inodes(&self, src: u64, dst: u64) -> Result<(Inode, Inode)> {
        self.require_writable_top_level()?;
        let source = self.read_inode(src)?;
        let target = self.read_inode(dst)?;
        if !source.is_regular_file() || !target.is_regular_file() {
            return Err(Error::NotAFile);
        }
        crate::verity::refuse_mutation(dst, target.flags)?;
        if (source.flags ^ target.flags) & INODE_NODATASUM != 0 {
            return Err(refused(format!(
                "inode {src} and inode {dst} differ in whether they checksum their data, and \
                 a clone between them would leave {dst} partly checksummed"
            )));
        }
        if !self.sb.has_no_holes() {
            return Err(refused(
                "a clone on a volume without the no-holes feature records the holes in its \
                 range as items, which is not written yet"
                    .into(),
            ));
        }
        Ok((source, target))
    }

    /// Put clones of `src`'s items over `span` from `src_offset` into
    /// `dst` at `dst_offset`, replacing what `dst` holds there, as one
    /// committed transaction, moving `dst`'s change and modification
    /// times as the kernel's clone moves them.
    #[allow(clippy::too_many_arguments)]
    fn remap(
        &mut self,
        src: u64,
        src_offset: u64,
        dst: u64,
        target: &Inode,
        dst_offset: u64,
        span: Span,
    ) -> Result<()> {
        let Span { len, span } = span;
        let dst_items = extent_items(self.items_of(dst)?);
        refuse_inline_destination(dst, &dst_items)?;
        let new_size = target.size.max(dst_offset + len);
        let sector = u64::from(self.sb.sectorsize);
        if new_size > target.size && !target.size.is_multiple_of(sector) && !dst_items.is_empty() {
            return Err(refused(format!(
                "inode {dst} ends partway through a sector at {}, and growing it means zeroing \
                 the rest of that sector, which is not written yet",
                target.size
            )));
        }

        // What the destination holds in the range goes first; a clone's
        // item put under the key of one deleted replaces it.
        let cut = cut_range(dst, &dst_items, dst_offset, dst_offset + span)?;
        let mut edits = Edits::new();
        for edit in cut.edits {
            edits.insert(order(&edit.key()), edit);
        }
        let (releases, unrefs) = self.drop_covered(dst, &cut.covered)?;
        for (k, _) in &cut.covered {
            edits.insert(order(k), ItemEdit::Delete(*k));
        }
        let mut shares = Vec::new();
        for (bytenr, ref_offset) in cut.splits {
            let (_, extent_len, _) = self.extent_item(bytenr)?;
            self.refuse_keyed_data_refs(bytenr)?;
            shares.push(DataShare {
                bytenr,
                len: extent_len,
                ref_offset,
            });
        }

        let src_items = if src == dst {
            dst_items.clone()
        } else {
            extent_items(self.items_of(src)?)
        };
        for (k, d) in &src_items {
            let Some(clone) = self.clone_item(src, k, d, src_offset, span, dst_offset)? else {
                continue;
            };
            shares.push(clone.share);
            let key = DiskKey {
                objectid: dst,
                key_type: EXTENT_DATA_KEY,
                offset: clone.file_offset,
            };
            edits.insert(
                order(&key),
                ItemEdit::Put(OwnedItem {
                    key,
                    data: clone.data,
                }),
            );
        }
        let nbytes = counted_bytes(&dst_items, &edits)?;
        let released: Vec<(u64, u64)> = releases
            .iter()
            .map(|r| (r.old, r.old + r.old_len))
            .collect();
        let csum_edits = if released.is_empty() {
            Vec::new()
        } else {
            self.csum_edits(&released, &[], false)?
        };

        let generation = self.next_generation()?;
        let now = crate::namespace::now();
        let mut raw = self.raw_inode(dst)?;
        raw[io::SIZE..io::SIZE + 8].copy_from_slice(&new_size.to_le_bytes());
        raw[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME, io::MTIME]);
        let mut edits: Vec<ItemEdit> = edits.into_values().collect();
        edits.push(ItemEdit::Put(OwnedItem {
            key: DiskKey {
                objectid: dst,
                key_type: INODE_ITEM_KEY,
                offset: 0,
            },
            data: raw,
        }));

        let write = DataWrite {
            root: objectid::FS_TREE,
            ino: dst,
            time: now,
            edits,
            csum_edits,
            releases,
            unrefs,
            shares,
            ..Default::default()
        };
        self.commit_write(write, generation)
    }

    /// What deleting the destination's items wholly inside a clone's range
    /// does to their extents: the releases of those the deleted items were
    /// the only references to, and the references dropped from those
    /// anything else still holds -- another file, another item of this
    /// one, or a leaf a snapshot shares, as [`Filesystem::truncate`]
    /// decides it.
    fn drop_covered(
        &self,
        dst: u64,
        covered: &[(DiskKey, Vec<u8>)],
    ) -> Result<(Vec<DataRelease>, Vec<DataRelease>)> {
        let mut by_extent: BTreeMap<u64, Vec<&(DiskKey, Vec<u8>)>> = BTreeMap::new();
        for item in covered {
            let bytenr = le64(&item.1, fe::DISK_BYTENR);
            if bytenr != 0 {
                by_extent.entry(bytenr).or_default().push(item);
            }
        }
        let mut releases = Vec::new();
        let mut unrefs = Vec::new();
        for (bytenr, items) in by_extent {
            let (refs, extent_len, _) = self.extent_item(bytenr)?;
            let mut drops = Vec::new();
            let mut leaf_shared = false;
            for (k, d) in items {
                let offset = le64(d, fe::OFFSET);
                let num = le64(d, fe::NUM_BYTES);
                // A compressed extent's window is in its decoded bytes,
                // which its on-disk length does not bound.
                if d[fe::COMPRESSION] == 0
                    && !window_inside_extent(bytenr + offset, num, bytenr, extent_len)
                {
                    return Err(refused(format!(
                        "inode {dst}: the item at {} references [{}, +{num}), outside the \
                         {extent_len}-byte extent the extent tree records at {bytenr}",
                        k.offset,
                        bytenr + offset
                    )));
                }
                drops.push(DataRelease {
                    old: bytenr,
                    old_len: extent_len,
                    // The kernel's subtraction, which wraps for a window
                    // further into its extent than its file offset.
                    old_ref_offset: k.offset.wrapping_sub(offset),
                });
                leaf_shared |= self.leaf_shared(self.written_tree(), k)?;
            }
            if refs == drops.len() as u64 && !leaf_shared {
                releases.extend(drops);
            } else {
                unrefs.extend(drops);
            }
        }
        Ok((releases, unrefs))
    }

    /// The range a clone covers, checked against the kernel's rules, or
    /// `None` when it is empty.
    #[allow(clippy::too_many_arguments)]
    fn clone_span(
        &self,
        src: u64,
        source: &crate::inode::Inode,
        src_offset: u64,
        dst: u64,
        target: &crate::inode::Inode,
        dst_offset: u64,
        len: u64,
    ) -> Result<Option<Span>> {
        let sector = u64::from(self.sb.sectorsize);
        if src_offset > source.size {
            return Err(refused(format!(
                "the clone starts at {src_offset}, past the end of inode {src} at {}",
                source.size
            )));
        }
        let len = if len == 0 {
            source.size - src_offset
        } else {
            len
        };
        let src_end = src_offset
            .checked_add(len)
            .filter(|&end| end <= source.size)
            .ok_or_else(|| {
                refused(format!(
                    "the clone of {len} bytes from {src_offset} runs past the end of inode \
                     {src} at {}",
                    source.size
                ))
            })?;
        if !src_offset.is_multiple_of(sector) || !dst_offset.is_multiple_of(sector) {
            return Err(refused(format!(
                "a clone's offsets must be whole {sector}-byte sectors, and {src_offset} and \
                 {dst_offset} are not both"
            )));
        }
        let to_end = src_end == source.size;
        if !len.is_multiple_of(sector) && !to_end {
            return Err(refused(format!(
                "a clone of {len} bytes is not a whole number of {sector}-byte sectors and \
                 does not run to the end of inode {src}"
            )));
        }
        if len == 0 {
            return Ok(None);
        }
        // The source's last sector is cloned whole when the range runs to
        // its end, and must not land inside the destination, where the
        // bytes past the source's end would read as the destination's.
        let span = src_end.next_multiple_of(sector) - src_offset;
        let dst_end = dst_offset.checked_add(span).ok_or_else(|| {
            refused(format!(
                "a clone of {span} bytes at {dst_offset} runs past the largest file offset"
            ))
        })?;
        if !len.is_multiple_of(sector) && dst_offset + len < target.size {
            return Err(refused(format!(
                "the clone ends partway through the last sector of inode {src}, and would put \
                 that sector inside inode {dst}, which runs on to {}",
                target.size
            )));
        }
        if src == dst && src_offset < dst_end && dst_offset < src_offset + span {
            return Err(refused(format!(
                "a clone of inode {src} into itself from {src_offset} to {dst_offset} overlaps \
                 the range it reads"
            )));
        }
        Ok(Some(Span { len, span }))
    }

    /// The destination item one source item becomes, or `None` for an
    /// item outside the range or a hole.
    fn clone_item(
        &self,
        src: u64,
        k: &DiskKey,
        d: &[u8],
        src_offset: u64,
        span: u64,
        dst_offset: u64,
    ) -> Result<Option<ClonedItem>> {
        let start = k.offset;
        let src_end = src_offset + span;
        if d.get(fe::TYPE) == Some(&INLINE) {
            // An inline item holds the file's first sector.
            if src_offset == 0 {
                return Err(refused(format!(
                    "inode {src} holds its data inline, and cloning inline data is not \
                     implemented"
                )));
            }
            return Ok(None);
        }
        if d.len() != fe::REGULAR_SIZE || !matches!(d[fe::TYPE], 1 | 2) {
            return Err(refused(format!(
                "inode {src}: the extent item at {start} is of type {:?} and {} bytes, which \
                 cloning does not handle",
                d.get(fe::TYPE),
                d.len()
            )));
        }
        let num = le64(d, fe::NUM_BYTES);
        let end = start.checked_add(num).ok_or_else(|| {
            refused(format!(
                "inode {src}: the extent item at {start} runs past the largest file offset"
            ))
        })?;
        if end <= src_offset || start >= src_end {
            return Ok(None);
        }
        if d[fe::ENCRYPTION] != 0 || d[fe::OTHER_ENCODING..fe::OTHER_ENCODING + 2] != [0, 0] {
            return Err(refused(format!(
                "inode {src}: the extent item at {start} is encrypted or otherwise encoded"
            )));
        }
        let bytenr = le64(d, fe::DISK_BYTENR);
        // A hole on a no-holes volume: the destination's own implicit
        // hole says the same.
        if bytenr == 0 {
            return Ok(None);
        }
        let from = start.max(src_offset);
        let to = end.min(src_end);
        let offset = le64(d, fe::OFFSET) + (from - start);
        let file_offset = dst_offset + (from - src_offset);

        let (_, extent_len, _) = self.extent_item(bytenr)?;
        // A compressed extent's window is in its decoded bytes, which its
        // on-disk length does not bound.
        if d[fe::COMPRESSION] == 0
            && !window_inside_extent(bytenr + offset, to - from, bytenr, extent_len)
        {
            return Err(refused(format!(
                "inode {src}: the item at {start} references [{}, +{}), outside the \
                 {extent_len}-byte extent the extent tree records at {bytenr}",
                bytenr + offset,
                to - from
            )));
        }
        self.refuse_keyed_data_refs(bytenr)?;

        let mut data = d.to_vec();
        data[fe::OFFSET..fe::OFFSET + 8].copy_from_slice(&offset.to_le_bytes());
        data[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&(to - from).to_le_bytes());
        Ok(Some(ClonedItem {
            file_offset,
            data,
            share: DataShare {
                bytenr,
                len: extent_len,
                // The kernel's subtraction, which wraps when the window
                // starts further into the extent than its file offset.
                ref_offset: file_offset.wrapping_sub(offset),
            },
        }))
    }

    /// Refuse an extent whose references are kept, at least in part, as
    /// items of their own: the kernel adds no inline reference to one, and
    /// filing a new reference as an item is not written.
    fn refuse_keyed_data_refs(&self, bytenr: u64) -> Result<()> {
        let root = self.tree_root(objectid::EXTENT_TREE)?;
        let reader = self.pool_reader();
        let tree = reader.tree();
        for kind in [EXTENT_DATA_REF, SHARED_DATA_REF] {
            if !tree.find_all(root, bytenr, kind)?.is_empty() {
                return Err(refused(format!(
                    "the data extent at {bytenr} keeps references as items of their own, and \
                     filing one that way is not implemented"
                )));
            }
        }
        Ok(())
    }
}

/// How much a clone covers: `len` bytes of file, and `span` bytes of
/// extent items -- `len` rounded up to the sector when it runs to the
/// source's end.
#[derive(Debug, Clone, Copy)]
struct Span {
    len: u64,
    span: u64,
}

/// One destination item a clone puts, and the reference it adds.
struct ClonedItem {
    file_offset: u64,
    data: Vec<u8>,
    share: DataShare,
}

/// Refuse a destination with inline data, or with an extent item too
/// short to read.
fn refuse_inline_destination(dst: u64, items: &[(DiskKey, Vec<u8>)]) -> Result<()> {
    for (k, d) in items {
        if d.get(fe::TYPE) == Some(&INLINE) {
            return Err(refused(format!(
                "inode {dst} holds inline data, and a clone into it means moving that data \
                 into an extent, which is not written yet"
            )));
        }
        if d.len() < fe::REGULAR_SIZE || !matches!(d[fe::TYPE], 1 | 2) {
            return Err(refused(format!(
                "inode {dst}: the extent item at {} is of type {:?} and {} bytes, which \
                 cloning does not handle",
                k.offset,
                d.get(fe::TYPE),
                d.len()
            )));
        }
    }
    Ok(())
}

/// What replacing `[lo, hi)` does to a destination's own items, before
/// the clone's are put there.
#[derive(Debug, Default, PartialEq, Eq)]
struct Cut {
    /// Items that run into or out of the range, or through it: each cut
    /// short at the range's start, started again at its end -- under a new
    /// key, the old one deleted -- or both, as two items.
    edits: Vec<ItemEdit>,
    /// Items wholly inside the range, which the clone deletes.
    covered: Vec<(DiskKey, Vec<u8>)>,
    /// For each item split in two, the extent its halves both name and
    /// the reference offset they share: the second half adds one to it.
    splits: Vec<(u64, u64)>,
}

/// What replacing `[lo, hi)` of inode `dst` does to each of its
/// `EXTENT_DATA` items, as the kernel's clone drops the range.
///
/// An item keeps its reference offset, its key offset less its offset into
/// the extent, however it is cut, since both move together.
fn cut_range(dst: u64, items: &[(DiskKey, Vec<u8>)], lo: u64, hi: u64) -> Result<Cut> {
    let mut cut = Cut::default();
    for (k, d) in items {
        let start = k.offset;
        let num = le64(d, fe::NUM_BYTES);
        let end = start.checked_add(num).ok_or_else(|| {
            refused(format!(
                "inode {dst}: the extent item at {start} runs past the largest file offset"
            ))
        })?;
        if end <= lo || start >= hi {
            continue;
        }
        if lo <= start && end <= hi {
            cut.covered.push((*k, d.clone()));
            continue;
        }
        let bytenr = le64(d, fe::DISK_BYTENR);
        let offset = le64(d, fe::OFFSET);
        if start < lo {
            let mut front = d.clone();
            front[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&(lo - start).to_le_bytes());
            cut.edits.push(ItemEdit::Put(OwnedItem {
                key: *k,
                data: front,
            }));
        } else {
            cut.edits.push(ItemEdit::Delete(*k));
        }
        if end > hi {
            let mut back = d.clone();
            back[fe::OFFSET..fe::OFFSET + 8]
                .copy_from_slice(&(offset + (hi - start)).to_le_bytes());
            back[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&(end - hi).to_le_bytes());
            cut.edits.push(ItemEdit::Put(OwnedItem {
                key: DiskKey { offset: hi, ..*k },
                data: back,
            }));
            if start < lo && bytenr != 0 {
                cut.splits.push((bytenr, start.wrapping_sub(offset)));
            }
        }
    }
    Ok(cut)
}

/// A change's edits of a destination's items, by key, so a later edit of
/// one key replaces an earlier one.
type Edits = BTreeMap<(u64, u8, u64), ItemEdit>;

/// A key in the order the tree sorts it.
fn order(k: &DiskKey) -> (u64, u8, u64) {
    (k.objectid, k.key_type, k.offset)
}

/// The inode's `nbytes` once `edits` apply to its `EXTENT_DATA` items:
/// every item that has an extent at its `num_bytes`, as `btrfs check`
/// counts it. Inline items are refused before this is asked.
fn counted_bytes(items: &[(DiskKey, Vec<u8>)], edits: &Edits) -> Result<u64> {
    let mut after: BTreeMap<(u64, u8, u64), &[u8]> = items
        .iter()
        .map(|(k, d)| (order(k), d.as_slice()))
        .collect();
    for (at, edit) in edits {
        match edit {
            ItemEdit::Delete(_) => after.remove(at),
            ItemEdit::Put(item) => after.insert(*at, item.data.as_slice()),
        };
    }
    after.values().try_fold(0u64, |total, d| {
        let bytes = if le64(d, fe::DISK_BYTENR) == 0 {
            0
        } else {
            le64(d, fe::NUM_BYTES)
        };
        total
            .checked_add(bytes)
            .ok_or_else(|| refused("the destination's byte count would overflow".into()))
    })
}

/// The `EXTENT_DATA` items among an inode's.
fn extent_items(items: Vec<(DiskKey, Vec<u8>)>) -> Vec<(DiskKey, Vec<u8>)> {
    items
        .into_iter()
        .filter(|(k, _)| k.key_type == EXTENT_DATA_KEY)
        .collect()
}

fn refused(why: String) -> Error {
    Error::UnsupportedFeature(why)
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(offset: u64, kind: u8, num: u64) -> (DiskKey, Vec<u8>) {
        let mut d = vec![0u8; fe::REGULAR_SIZE];
        d[fe::TYPE] = kind;
        d[fe::DISK_BYTENR..fe::DISK_BYTENR + 8].copy_from_slice(&(1u64 << 20).to_le_bytes());
        d[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&num.to_le_bytes());
        (
            DiskKey {
                objectid: 257,
                key_type: EXTENT_DATA_KEY,
                offset,
            },
            d,
        )
    }

    /// `item` with its offset into the extent set.
    fn windowed(offset: u64, num: u64, within: u64) -> (DiskKey, Vec<u8>) {
        let (k, mut d) = item(offset, 1, num);
        d[fe::OFFSET..fe::OFFSET + 8].copy_from_slice(&within.to_le_bytes());
        (k, d)
    }

    fn put((key, data): (DiskKey, Vec<u8>)) -> ItemEdit {
        ItemEdit::Put(OwnedItem { key, data })
    }

    #[test]
    fn a_destination_range_clear_of_every_item_cuts_nothing() {
        let items = vec![item(0, 1, 8192), item(16384, 2, 4096)];
        assert_eq!(cut_range(257, &items, 8192, 16384).unwrap(), Cut::default());
        assert_eq!(
            cut_range(257, &items, 20480, 24576).unwrap(),
            Cut::default()
        );
    }

    #[test]
    fn items_over_a_destination_range_are_cut_short_started_again_split_or_deleted() {
        // One item runs into the range, one lies inside it and one runs
        // out of it.
        let items = vec![
            item(0, 1, 8192),
            item(8192, 2, 4096),
            windowed(12288, 8192, 4096),
        ];
        let cut = cut_range(257, &items, 4096, 16384).unwrap();
        assert_eq!(
            cut,
            Cut {
                edits: vec![
                    put(item(0, 1, 4096)),
                    ItemEdit::Delete(items[2].0),
                    put(windowed(16384, 4096, 8192)),
                ],
                covered: vec![items[1].clone()],
                splits: Vec::new(),
            }
        );

        // One item runs through the range: two halves naming one extent
        // at one reference offset, the second adding a reference to it.
        let through = vec![windowed(4096, 16384, 8192)];
        let cut = cut_range(257, &through, 8192, 12288).unwrap();
        assert_eq!(
            cut,
            Cut {
                edits: vec![
                    put(windowed(4096, 4096, 8192)),
                    put(windowed(12288, 8192, 16384))
                ],
                covered: Vec::new(),
                splits: vec![(1 << 20, 4096u64.wrapping_sub(8192))],
            }
        );
    }

    #[test]
    fn a_destination_holding_inline_data_is_refused() {
        let (k, mut d) = item(0, 1, 4096);
        d[fe::TYPE] = INLINE;
        let err = refuse_inline_destination(257, &[(k, d)]).unwrap_err();
        assert!(err.to_string().contains("inline"), "{err}");
    }

    #[test]
    fn nbytes_counts_the_items_left_and_put_but_not_holes() {
        let items = vec![item(0, 1, 8192), item(8192, 1, 4096)];
        let mut hole = item(12288, 1, 4096);
        hole.1[fe::DISK_BYTENR..fe::DISK_BYTENR + 8].copy_from_slice(&0u64.to_le_bytes());
        let edits: Edits = [
            ItemEdit::Delete(items[1].0),
            put(hole.clone()),
            put(item(16384, 1, 4096)),
        ]
        .into_iter()
        .map(|e| (order(&e.key()), e))
        .collect();
        assert_eq!(counted_bytes(&items, &edits).unwrap(), 12288);
    }
}
