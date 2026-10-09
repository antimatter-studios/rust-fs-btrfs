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
//!   kernel does; an inline item is cut to the new length.
//! - **Growing** moves only the inode's size, which needs the `NO_HOLES`
//!   feature: without it the kernel records the new range as a hole item.
//!
//! Either way the inode's `nbytes` is recounted from the items left (inline
//! data at its length, every other item with an extent at its `num_bytes`,
//! as `btrfs check` counts it), and its change and modification times move.
//!
//! # What it refuses
//!
//! - an extent another reference shares — a snapshot, a reflink, or a
//!   second item of the same file: releasing one reference of several is
//!   not written yet (#261);
//! - growing a file whose last sector is partly past its end, or that holds
//!   inline data: the bytes between the old end and the sector boundary
//!   must read as zeros, which means writing that sector;
//! - a compressed inline extent cut short, and any subvolume other than the
//!   top level.

use crate::chunk::DiskKey;
use crate::error::{Error, Result};
use crate::fs::{file_extent as fe, Filesystem, EXTENT_DATA_KEY};
use crate::inode::{offsets as io, INODE_ITEM_KEY};
use crate::leaf_edit::OwnedItem;
use crate::transaction::{DataRelease, DataWrite, ItemEdit};
use crate::write::window_inside_extent;

/// `BTRFS_FILE_EXTENT_INLINE`.
const INLINE: u8 = 0;

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
    /// everything is.
    pub fn truncate(&mut self, ino: u64, size: u64) -> Result<()> {
        self.require_writable_top_level()?;
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() {
            return Err(Error::NotAFile);
        }
        if size == inode.size {
            return Ok(());
        }
        let sector = u64::from(self.sb.sectorsize);
        let items: Vec<(DiskKey, Vec<u8>)> = self
            .items_of(ino)?
            .into_iter()
            .filter(|(k, _)| k.key_type == EXTENT_DATA_KEY)
            .collect();

        let mut edits = Vec::new();
        let mut releases = Vec::new();
        let mut released = Vec::new();
        if size > inode.size {
            self.refuse_growth(ino, inode.size, &items)?;
        } else {
            let cut = size.next_multiple_of(sector);
            let last_snapshot = self.fs_tree_last_snapshot()?;
            for (k, d) in &items {
                if let Some(edit) =
                    self.shrink_item(ino, size, cut, last_snapshot, k, d, &mut releases)?
                {
                    edits.push(edit);
                }
            }
            released = releases
                .iter()
                .map(|r: &DataRelease| (r.old, r.old + r.old_len))
                .collect();
        }

        let generation = self.next_generation()?;
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
            time: now,
            edits,
            csum_edits: if released.is_empty() {
                Vec::new()
            } else {
                self.csum_edits(&released, &[], false)?
            },
            releases,
        };
        self.commit_write(write, generation)
    }

    /// Refuse growing a file whose new bytes could not be read as zeros
    /// without writing data.
    fn refuse_growth(&self, ino: u64, old: u64, items: &[(DiskKey, Vec<u8>)]) -> Result<()> {
        if !self.sb.has_no_holes() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: growing a file on a volume without the no-holes feature means \
                 recording the new range as a hole item, which is not written yet"
            )));
        }
        if items.iter().any(|(_, d)| d.get(fe::TYPE) == Some(&INLINE)) {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} holds inline data, and growing it means moving that data into \
                 an extent, which is not written yet"
            )));
        }
        if !old.is_multiple_of(u64::from(self.sb.sectorsize)) && !items.is_empty() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} ends partway through a sector at {old}, and growing it means \
                 zeroing the rest of that sector, which is not written yet"
            )));
        }
        Ok(())
    }

    /// What shrinking to `size` does to one `EXTENT_DATA` item: nothing,
    /// a shorter item, or its deletion — with its extent released when
    /// this item was its one reference.
    #[allow(clippy::too_many_arguments)]
    fn shrink_item(
        &self,
        ino: u64,
        size: u64,
        cut: u64,
        last_snapshot: u64,
        k: &DiskKey,
        d: &[u8],
        releases: &mut Vec<DataRelease>,
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
                if !unencoded(d) || d.len() != fe::INLINE_DATA + ram as usize {
                    return Err(Error::UnsupportedFeature(format!(
                        "inode {ino}: the inline extent at {start} is compressed or encoded, \
                         and cutting one short is not implemented"
                    )));
                }
                let keep = (size - start) as usize;
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
                    let (refs, extent_len, generation) = self.extent_item(bytenr)?;
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
                    if refs != 1 {
                        return Err(Error::UnsupportedFeature(format!(
                            "inode {ino}: the extent at {bytenr} has {refs} references — a \
                             snapshot, a reflink or another item of this file — and \
                             releasing one reference of several is not implemented"
                        )));
                    }
                    if generation <= last_snapshot {
                        return Err(Error::UnsupportedFeature(format!(
                            "inode {ino}: the extent at {bytenr} is from generation \
                             {generation}, at or before the snapshot taken at {last_snapshot}, \
                             so the snapshot may still be reading it"
                        )));
                    }
                    releases.push(DataRelease {
                        old: bytenr,
                        old_len: extent_len,
                        old_ref_offset: start - offset,
                    });
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
}
