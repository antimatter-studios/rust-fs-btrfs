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
//! not change. The destination's size and `nbytes` grow by what it gains,
//! and its change and modification times move, as the kernel's clone
//! moves them. It is one committed transaction on the top-level
//! subvolume, through the same planner and commit as
//! [`crate::truncate`].
//!
//! The kernel's rules for the range hold here too: both offsets are whole
//! sectors, and so is the length unless it runs to the source's end, in
//! which case the source's last, partial sector is cloned whole and must
//! not land inside the destination. A length of zero clones to the
//! source's end, as `FICLONERANGE` does. A file may be cloned into itself
//! where the two ranges do not overlap.
//!
//! A write into either copy afterwards is refused, as a write into any
//! shared extent is (#261); truncating or unlinking one drops its
//! references and leaves the extents to the other (#262).
//!
//! # What it refuses
//!
//! - a destination range that already holds data, or a destination with
//!   inline data: replacing what is there means releasing it, which a
//!   clone does not do yet;
//! - inline data in the source range, and encrypted or otherwise encoded
//!   extents;
//! - an extent whose references are kept as items of their own, or would
//!   be once this one is added -- the kernel files them so past about 33
//!   references at a 16 KiB node -- since filing a reference that way is
//!   not written;
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

use crate::backref::{EXTENT_DATA_REF, SHARED_DATA_REF};
use crate::chunk::{objectid, DiskKey};
use crate::error::{Error, Result};
use crate::fs::{file_extent as fe, Filesystem, EXTENT_DATA_KEY};
use crate::inode::{offsets as io, INODE_ITEM_KEY, INODE_NODATASUM};
use crate::leaf_edit::OwnedItem;
use crate::transaction::{DataShare, DataWrite, ItemEdit};
use crate::write::window_inside_extent;

/// `BTRFS_FILE_EXTENT_INLINE`.
const INLINE: u8 = 0;

impl Filesystem {
    /// Clone `len` bytes of regular file `src`, from `src_offset`, into
    /// regular file `dst` at `dst_offset`, as one committed transaction:
    /// the two then share those extents. A `len` of zero clones to the end
    /// of `src`. `src` and `dst` may be the same file when the two ranges
    /// do not overlap.
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
        self.require_writable_top_level()?;
        let source = self.read_inode(src)?;
        let target = self.read_inode(dst)?;
        if !source.is_regular_file() || !target.is_regular_file() {
            return Err(Error::NotAFile);
        }
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

        let range = self.clone_span(src, &source, src_offset, dst, &target, dst_offset, len)?;
        let Some(Span { len, span }) = range else {
            return Ok(());
        };
        let dst_items = extent_items(self.items_of(dst)?);
        refuse_destination(dst, &dst_items, dst_offset, span)?;
        let new_size = target.size.max(dst_offset + len);
        let sector = u64::from(self.sb.sectorsize);
        if new_size > target.size && !target.size.is_multiple_of(sector) && !dst_items.is_empty() {
            return Err(refused(format!(
                "inode {dst} ends partway through a sector at {}, and growing it means zeroing \
                 the rest of that sector, which is not written yet",
                target.size
            )));
        }

        let src_items = if src == dst {
            dst_items
        } else {
            extent_items(self.items_of(src)?)
        };
        let mut edits = Vec::new();
        let mut shares = Vec::new();
        let mut gained = 0u64;
        for (k, d) in &src_items {
            let Some(clone) = self.clone_item(src, k, d, src_offset, span, dst_offset)? else {
                continue;
            };
            gained += clone.num_bytes;
            shares.push(clone.share);
            edits.push(ItemEdit::Put(OwnedItem {
                key: DiskKey {
                    objectid: dst,
                    key_type: EXTENT_DATA_KEY,
                    offset: clone.file_offset,
                },
                data: clone.data,
            }));
        }

        let generation = self.next_generation()?;
        let now = crate::namespace::now();
        let mut raw = self.raw_inode(dst)?;
        let nbytes = le64(&raw, io::NBYTES)
            .checked_add(gained)
            .ok_or_else(|| refused(format!("inode {dst}'s byte count would overflow")))?;
        raw[io::SIZE..io::SIZE + 8].copy_from_slice(&new_size.to_le_bytes());
        raw[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME, io::MTIME]);
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
            shares,
            ..Default::default()
        };
        self.commit_write(write, generation)
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
            num_bytes: to - from,
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
struct Span {
    len: u64,
    span: u64,
}

/// One destination item a clone puts, and the reference it adds.
struct ClonedItem {
    file_offset: u64,
    num_bytes: u64,
    data: Vec<u8>,
    share: DataShare,
}

/// Refuse a destination with inline data, or with any extent item over
/// `[dst_offset, dst_offset + span)`.
fn refuse_destination(
    dst: u64,
    items: &[(DiskKey, Vec<u8>)],
    dst_offset: u64,
    span: u64,
) -> Result<()> {
    let dst_end = dst_offset + span;
    for (k, d) in items {
        if d.get(fe::TYPE) == Some(&INLINE) {
            return Err(refused(format!(
                "inode {dst} holds inline data, and a clone into it means moving that data \
                 into an extent, which is not written yet"
            )));
        }
        let num = d
            .get(fe::NUM_BYTES..fe::NUM_BYTES + 8)
            .map(|_| le64(d, fe::NUM_BYTES))
            .ok_or_else(|| {
                refused(format!(
                    "inode {dst}: the extent item at {} is {} bytes, too short to read",
                    k.offset,
                    d.len()
                ))
            })?;
        if k.offset < dst_end && dst_offset < k.offset.saturating_add(num) {
            return Err(refused(format!(
                "inode {dst} already holds data at {} in the range cloned into, and replacing \
                 it is not implemented",
                k.offset
            )));
        }
    }
    Ok(())
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

    #[test]
    fn a_destination_range_clear_of_every_item_is_accepted() {
        let items = vec![item(0, 1, 8192), item(16384, 2, 4096)];
        refuse_destination(257, &items, 8192, 8192).unwrap();
        refuse_destination(257, &items, 20480, 4096).unwrap();
    }

    #[test]
    fn a_destination_range_over_an_item_is_refused() {
        let items = vec![item(0, 1, 8192), item(16384, 2, 4096)];
        for (at, span) in [(4096, 4096), (12288, 8192), (0, 1 << 20)] {
            let err = refuse_destination(257, &items, at, span).unwrap_err();
            assert!(err.to_string().contains("already holds data"), "{err}");
        }
    }

    #[test]
    fn a_destination_holding_inline_data_is_refused() {
        let (k, mut d) = item(0, 1, 4096);
        d[fe::TYPE] = INLINE;
        let err = refuse_destination(257, &[(k, d)], 1 << 20, 4096).unwrap_err();
        assert!(err.to_string().contains("inline"), "{err}");
    }
}
