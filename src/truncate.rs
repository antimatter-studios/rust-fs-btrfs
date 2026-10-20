//! Changing a regular file's length: truncate (#262).
//!
//! A truncate is one committed transaction on the top-level subvolume,
//! through the same planner and commit as [`crate::namespace`]:
//!
//! - **Shrinking** deletes every `EXTENT_DATA` item that begins at or past
//!   the new end, rounded up to a sector, and releases each extent such an
//!   item was the only reference to: its `EXTENT_ITEM` goes, the free-space
//!   tree and the block group's `used` give the space back, and its digests
//!   leave the checksum tree. An item that straddles the new end keeps its
//!   extent whole and references less of it (`num_bytes` shrinks), as the
//!   kernel does; an inline item is cut to the new length, a compressed
//!   one decoded and the bytes kept written back uncompressed (#423).
//! - **Growing** moves the inode's size. With the `NO_HOLES` feature the
//!   new range is an implicit hole; without it every range up to the size,
//!   rounded up to a sector, must have an item, so each part of the new
//!   range no item covers gets a hole item, a regular extent item whose disk
//!   bytenr is 0, as the kernel records it (#420). A file that ends
//!   partway through a sector first has the rest of that sector rewritten
//!   copy-on-write with zeros (#421), through [`Filesystem::write`], so
//!   stale bytes past the old end never read back. A file that holds
//!   inline data first has that data moved into a regular extent at
//!   offset 0, the rest of its sector zeros (#422), as the kernel does:
//!   an inline extent never stays shorter than its file.
//!
//! Either way the inode's `nbytes` is recounted from the items left (inline
//! data at its length, every other item with an extent at its `num_bytes`,
//! as `btrfs check` counts it), and its change and modification times move.
//!
//! An extent something else still reads stays, and only this file's
//! reference to it goes (#262): one a reflink shares, counting more than
//! one reference, and one a snapshot shares, whose file item sits in a leaf
//! the snapshot reaches too. Copying that leaf out of the snapshot's way
//! gives the extent a reference naming the old leaf, as the kernel's
//! copy-on-write does, so the snapshot keeps reading it.
//!
//! # What it refuses
//!
//! - an extent whose reference from this file is held anywhere but inline
//!   in the extent's record;
//! - on a volume without `NO_HOLES`, growing a file whose last sector is
//!   partly past its end, inline data included; and growing a `nodatacow`
//!   file that holds inline data.
//! - an encrypted or otherwise encoded inline extent cut short, a
//!   compressed one cut to a sector or more, and any subvolume other than
//!   the top level.

use crate::chunk::DiskKey;
use crate::error::{Error, Result};
use crate::fs::{file_extent as fe, Filesystem, EXTENT_DATA_KEY};
use crate::inode::{offsets as io, INODE_ITEM_KEY};
use crate::leaf_edit::OwnedItem;
use crate::transaction::{DataRelease, DataWrite, ItemEdit};
use crate::write::window_inside_extent;

/// `BTRFS_FILE_EXTENT_INLINE`.
const INLINE: u8 = 0;
/// `BTRFS_FILE_EXTENT_REG`.
const REGULAR: u8 = 1;

impl Filesystem {
    /// Set the length of regular file `ino` to `size`, as one committed
    /// transaction. Releases the extents that lie wholly past the new end;
    /// a size equal to the current one changes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but a regular file, and
    /// [`Error::UnsupportedFeature`] for every refusal the [module
    /// documentation](crate::truncate) lists. Nothing is written unless
    /// everything is, except that a file ending partway through a sector,
    /// or holding inline data, and grown past it commits the zeroed sector
    /// first: if the growth after it then fails, the file ends at that
    /// sector's boundary.
    pub fn truncate(&mut self, ino: u64, size: u64) -> Result<()> {
        self.require_writable_top_level()?;
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() {
            return Err(Error::NotAFile);
        }
        crate::verity::refuse_mutation(ino, inode.flags)?;
        if size == inode.size {
            return Ok(());
        }
        let sector = u64::from(self.sb.sectorsize);
        let items: Vec<(DiskKey, Vec<u8>)> = self
            .items_of(ino)?
            .into_iter()
            .filter(|(k, _)| k.key_type == EXTENT_DATA_KEY)
            .collect();

        let generation = self.next_generation()?;
        let mut edits = Vec::new();
        let mut releases = Vec::new();
        let mut unrefs = Vec::new();
        let mut released = Vec::new();
        if size > inode.size {
            self.refuse_growth(ino, inode.size, &items)?;
            if items.iter().any(|(_, d)| d.get(fe::TYPE) == Some(&INLINE)) {
                // Inline data cannot stay shorter than its file, so it moves
                // into a regular extent at offset 0 (#422) that the write
                // fills to the next sector boundary with zeros; any growth
                // past that boundary is then sector-aligned.
                let boundary = (inode.size / sector + 1) * sector;
                let zeros = vec![0u8; (size.min(boundary) - inode.size) as usize];
                self.write_moving_inline(ino, inode.size, &zeros)?;
                return self.truncate(ino, size);
            }
            if !inode.size.is_multiple_of(sector) && !items.is_empty() {
                // The rest of the old last sector may hold stale bytes, so
                // it is rewritten copy-on-write with zeros past the old end,
                // as the kernel does; the write moves the size up to the
                // sector boundary or to `size`, whichever comes first, and
                // any growth past that boundary is then sector-aligned.
                let boundary = inode.size.next_multiple_of(sector);
                let zeros = vec![0u8; (size.min(boundary) - inode.size) as usize];
                self.write(ino, inode.size, &zeros)?;
                return self.truncate(ino, size);
            }
            if !self.sb.has_no_holes() {
                let from = inode.size.next_multiple_of(sector);
                let to = size.next_multiple_of(sector);
                edits.extend(hole_items(ino, from, to, &items, generation));
            }
        } else {
            let cut = size.next_multiple_of(sector);
            for (k, d) in &items {
                if let Some(edit) =
                    self.shrink_item(ino, size, cut, k, d, &mut releases, &mut unrefs)?
                {
                    edits.push(edit);
                }
            }
            released = releases
                .iter()
                .map(|r: &DataRelease| (r.old, r.old + r.old_len))
                .collect();
        }

        let now = crate::namespace::now();
        let mut raw = self.raw_inode(ino)?;
        raw[io::SIZE..io::SIZE + 8].copy_from_slice(&size.to_le_bytes());
        let nbytes = counted_bytes(&items, &edits)?;
        raw[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME, io::MTIME]);
        edits.push(ItemEdit::Put(OwnedItem {
            key: DiskKey {
                objectid: ino,
                key_type: INODE_ITEM_KEY,
                offset: 0,
            },
            data: raw,
        }));

        let write = DataWrite {
            root: crate::chunk::objectid::FS_TREE,
            ino,
            moves: Vec::new(),
            adds: Vec::new(),
            time: now,
            root_flags: None,
            edits,
            default_subvol: None,
            csum_edits: if released.is_empty() {
                Vec::new()
            } else {
                self.csum_edits(&released, &[], false)?
            },
            releases,
            unrefs,
            ..Default::default()
        };
        self.commit_write(write, generation)
    }

    /// Refuse growing a file whose new bytes could not be read as zeros
    /// without writing data.
    fn refuse_growth(&self, ino: u64, old: u64, items: &[(DiskKey, Vec<u8>)]) -> Result<()> {
        if !old.is_multiple_of(u64::from(self.sb.sectorsize))
            && !items.is_empty()
            && !self.sb.has_no_holes()
        {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} ends partway through a sector at {old}, and growing it on a \
                 volume without the no-holes feature means zeroing the rest of that sector \
                 with a write that keeps hole items, which is not implemented"
            )));
        }
        Ok(())
    }

    /// What shrinking to `size` does to one `EXTENT_DATA` item: nothing,
    /// a shorter item, or its deletion — with its extent released when
    /// this item was its one reference, and its reference dropped from the
    /// extent when a reflink or a snapshot shares it (#262).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn shrink_item(
        &self,
        ino: u64,
        size: u64,
        cut: u64,
        k: &DiskKey,
        d: &[u8],
        releases: &mut Vec<DataRelease>,
        unrefs: &mut Vec<DataRelease>,
    ) -> Result<Option<ItemEdit>> {
        let start = k.offset;
        let unencoded = |d: &[u8]| {
            d[fe::COMPRESSION] == 0
                && d[fe::ENCRYPTION] == 0
                && d[fe::OTHER_ENCODING..fe::OTHER_ENCODING + 2] == [0, 0]
        };
        match d.get(fe::TYPE) {
            Some(&INLINE) if d.len() >= fe::INLINE_DATA => {
                let ram = le64(d, fe::RAM_BYTES);
                if size <= start {
                    return Ok(Some(ItemEdit::Delete(*k)));
                }
                if size >= start + ram {
                    return Ok(None);
                }
                let keep = (size - start) as usize;
                let sector = self.sb.sectorsize as usize;
                if d[fe::COMPRESSION] != 0
                    && d[fe::ENCRYPTION] == 0
                    && d[fe::OTHER_ENCODING..fe::OTHER_ENCODING + 2] == [0, 0]
                {
                    // A compressed inline extent is decoded and the bytes
                    // kept are written back plain (#423): the kernel reads
                    // either shape, and a plain one cannot hold stale
                    // decoded bytes past the file's end. Kept short of a
                    // sector, which `btrfs check` requires of plain inline
                    // data.
                    if keep >= sector {
                        return Err(Error::UnsupportedFeature(format!(
                            "inode {ino}: the compressed inline extent at {start} would \
                             keep {keep} bytes, a sector or more, which inline data \
                             cannot hold uncompressed"
                        )));
                    }
                    let decoded = crate::compression::decompress(
                        crate::compression::Compression::from_byte(d[fe::COMPRESSION])?,
                        &d[fe::INLINE_DATA..],
                        ram as usize,
                        sector,
                        crate::compression::ShortDecode::Pad,
                    )?;
                    let mut data = d[..fe::INLINE_DATA].to_vec();
                    data[fe::COMPRESSION] = 0;
                    data[fe::RAM_BYTES..fe::RAM_BYTES + 8]
                        .copy_from_slice(&(keep as u64).to_le_bytes());
                    data.extend_from_slice(&decoded[..keep]);
                    return Ok(Some(ItemEdit::Put(OwnedItem { key: *k, data })));
                }
                if !unencoded(d) || d.len() != fe::INLINE_DATA + ram as usize {
                    return Err(Error::UnsupportedFeature(format!(
                        "inode {ino}: the inline extent at {start} is encrypted or encoded, \
                         or its length disagrees with its ram_bytes, and cutting one short \
                         is not implemented"
                    )));
                }
                let mut data = d[..fe::INLINE_DATA + keep].to_vec();
                data[fe::RAM_BYTES..fe::RAM_BYTES + 8]
                    .copy_from_slice(&(keep as u64).to_le_bytes());
                Ok(Some(ItemEdit::Put(OwnedItem { key: *k, data })))
            }
            Some(1 | 2) if d.len() >= fe::REGULAR_SIZE => {
                let num = le64(d, fe::NUM_BYTES);
                if start < cut {
                    if start + num <= cut {
                        return Ok(None);
                    }
                    // Straddles the new end: the extent stays whole, and
                    // the item references less of it.
                    let mut data = d.to_vec();
                    data[fe::NUM_BYTES..fe::NUM_BYTES + 8]
                        .copy_from_slice(&(cut - start).to_le_bytes());
                    return Ok(Some(ItemEdit::Put(OwnedItem { key: *k, data })));
                }
                let bytenr = le64(d, fe::DISK_BYTENR);
                if bytenr != 0 {
                    let offset = le64(d, fe::OFFSET);
                    let (refs, extent_len, _) = self.extent_item(bytenr)?;
                    // A compressed extent's window is in its decoded bytes,
                    // which its on-disk length does not bound.
                    let compressed = d[fe::COMPRESSION] != 0;
                    if !compressed
                        && !window_inside_extent(bytenr + offset, num, bytenr, extent_len)
                    {
                        return Err(Error::UnsupportedFeature(format!(
                            "inode {ino}: the item at {start} references [{}, +{num}), outside \
                             the {extent_len}-byte extent the extent tree records at {bytenr}",
                            bytenr + offset
                        )));
                    }
                    let release = DataRelease {
                        old: bytenr,
                        old_len: extent_len,
                        old_ref_offset: start - offset,
                    };
                    // Another reference is a file item a reflink made, or
                    // a snapshot's own copy of a leaf; a leaf the snapshot
                    // still shares counts none, and gains one naming it
                    // when this change copies it. Either way this item's
                    // reference is dropped and the extent stays for the
                    // rest (#262).
                    if refs == 1 && !self.leaf_shared(self.written_tree(), k)? {
                        releases.push(release);
                    } else {
                        unrefs.push(release);
                    }
                }
                Ok(Some(ItemEdit::Delete(*k)))
            }
            other => Err(Error::UnsupportedFeature(format!(
                "inode {ino}: the extent item at {start} is of type {other:?} and {} bytes, \
                 which truncating does not handle",
                d.len()
            ))),
        }
    }
}

/// A hole item, generation `generation`, for each part of `[from, to)` that
/// none of the file's extent `items` covers, on a volume without `NO_HOLES`
/// (#420). An extent preallocated past the old end keeps its range; a hole
/// is a regular extent item with disk bytenr 0 and its length as both
/// `num_bytes` and `ram_bytes`.
fn hole_items(
    ino: u64,
    from: u64,
    to: u64,
    items: &[(DiskKey, Vec<u8>)],
    generation: u64,
) -> Vec<ItemEdit> {
    let covered = items
        .iter()
        .filter(|(_, d)| d.len() >= fe::REGULAR_SIZE && d[fe::TYPE] != INLINE)
        .map(|(k, d)| (k.offset, k.offset.saturating_add(le64(d, fe::NUM_BYTES))));
    let mut gaps = Vec::new();
    let mut at = from;
    for (start, end) in covered {
        if start >= to {
            break;
        }
        if start > at {
            gaps.push((at, start));
        }
        at = at.max(end);
    }
    if at < to {
        gaps.push((at, to));
    }
    gaps.into_iter()
        .map(|(start, end)| {
            let len = (end - start).to_le_bytes();
            let mut data = vec![0u8; fe::REGULAR_SIZE];
            data[fe::GENERATION..fe::GENERATION + 8].copy_from_slice(&generation.to_le_bytes());
            data[fe::RAM_BYTES..fe::RAM_BYTES + 8].copy_from_slice(&len);
            data[fe::TYPE] = REGULAR;
            data[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&len);
            ItemEdit::Put(OwnedItem {
                key: DiskKey {
                    objectid: ino,
                    key_type: EXTENT_DATA_KEY,
                    offset: start,
                },
                data,
            })
        })
        .collect()
}

/// The inode's `nbytes` once `edits` apply to its extent items: inline data
/// at its length, and every other item that has an extent at its
/// `num_bytes` — the count `btrfs check` compares with the inode.
fn counted_bytes(items: &[(DiskKey, Vec<u8>)], edits: &[ItemEdit]) -> Result<u64> {
    let mut total = 0u64;
    for (k, d) in items {
        let d = match edits.iter().find(|e| e.key() == *k) {
            Some(ItemEdit::Delete(_)) => continue,
            Some(ItemEdit::Put(item)) => item.data.as_slice(),
            None => d.as_slice(),
        };
        total += match d.get(fe::TYPE) {
            Some(&INLINE) => le64(d, fe::RAM_BYTES),
            Some(1 | 2) if d.len() >= fe::REGULAR_SIZE && le64(d, fe::DISK_BYTENR) != 0 => {
                le64(d, fe::NUM_BYTES)
            }
            Some(1 | 2) => 0,
            _ => {
                return Err(Error::UnsupportedFeature(format!(
                    "an extent item at {} that truncating does not handle",
                    k.offset
                )))
            }
        };
    }
    Ok(total)
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular(bytenr: u64, num: u64) -> Vec<u8> {
        let mut d = vec![0u8; fe::REGULAR_SIZE];
        d[fe::TYPE] = 1;
        d[fe::DISK_BYTENR..fe::DISK_BYTENR + 8].copy_from_slice(&bytenr.to_le_bytes());
        d[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&num.to_le_bytes());
        d
    }

    fn key(offset: u64) -> DiskKey {
        DiskKey {
            objectid: 257,
            key_type: EXTENT_DATA_KEY,
            offset,
        }
    }

    #[test]
    fn nbytes_counts_extents_and_inline_data_but_not_holes() {
        let mut inline = vec![0u8; fe::INLINE_DATA];
        inline[fe::RAM_BYTES..fe::RAM_BYTES + 8].copy_from_slice(&5u64.to_le_bytes());
        inline.extend_from_slice(b"hello");
        let items = vec![
            (key(0), regular(1 << 20, 8192)),
            (key(8192), regular(0, 4096)),
            (key(12288), regular(2 << 20, 4096)),
        ];
        assert_eq!(counted_bytes(&items, &[]).unwrap(), 12288);
        let edits = vec![
            ItemEdit::Delete(key(12288)),
            ItemEdit::Put(OwnedItem {
                key: key(0),
                data: regular(1 << 20, 4096),
            }),
        ];
        assert_eq!(counted_bytes(&items, &edits).unwrap(), 4096);
        assert_eq!(counted_bytes(&[(key(0), inline)], &[]).unwrap(), 5);
    }

    /// Each hole item's `(offset, disk bytenr, num_bytes, ram_bytes)`.
    fn holes(from: u64, to: u64, items: &[(DiskKey, Vec<u8>)]) -> Vec<(u64, u64, u64, u64)> {
        hole_items(257, from, to, items, 9)
            .into_iter()
            .map(|e| match e {
                ItemEdit::Put(item) => {
                    assert_eq!(item.key.key_type, EXTENT_DATA_KEY);
                    assert_eq!(le64(&item.data, fe::GENERATION), 9);
                    assert_eq!(item.data[fe::TYPE], REGULAR);
                    (
                        item.key.offset,
                        le64(&item.data, fe::DISK_BYTENR),
                        le64(&item.data, fe::NUM_BYTES),
                        le64(&item.data, fe::RAM_BYTES),
                    )
                }
                ItemEdit::Delete(k) => panic!("a hole deletes {k:?}"),
            })
            .collect()
    }

    #[test]
    fn growth_holes_cover_only_what_no_item_does() {
        assert_eq!(holes(0, 1 << 20, &[]), vec![(0, 0, 1 << 20, 1 << 20)]);
        assert_eq!(
            holes(8192, 65536, &[(key(0), regular(1 << 20, 8192))]),
            vec![(8192, 0, 57344, 57344)]
        );
        // An extent preallocated past the old end keeps its range.
        let mut prealloc = regular(2 << 20, 8192);
        prealloc[fe::TYPE] = 2;
        assert_eq!(
            holes(
                8192,
                65536,
                &[
                    (key(0), regular(1 << 20, 8192)),
                    (key(16384), prealloc.clone())
                ]
            ),
            vec![(8192, 0, 8192, 8192), (24576, 0, 40960, 40960)]
        );
        // One that reaches past the new end leaves nothing to fill there.
        assert_eq!(
            holes(8192, 20480, &[(key(16384), prealloc)]),
            vec![(8192, 0, 8192, 8192)]
        );
        assert!(holes(8192, 8192, &[]).is_empty());
    }
}
