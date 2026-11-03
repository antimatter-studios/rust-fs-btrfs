//! Preallocating and punching a range of a file (#713).
//!
//! Each call is one committed transaction on the top-level subvolume,
//! through the same planner and commit as [`crate::clone`], and leaves the
//! items the kernel's `fallocate(2)` leaves.
//!
//! # Preallocating
//!
//! The range is rounded out to whole sectors. Each hole in it gets a new
//! extent, at most 256 MiB long as the kernel's allocation asks, named by an
//! `EXTENT_DATA` item of type `PREALLOC`, and the extent's record holds one
//! inline reference from that item. Such an extent holds no checksums, and
//! reads as zeros until written. What already holds data, preallocated or not,
//! is left alone. The inode gains `BTRFS_INODE_PREALLOC` when anything is
//! allocated, `nbytes` counts the new extents, and the size grows to the
//! range's end unless asked not to.
//!
//! # Punching
//!
//! Items wholly inside the range are deleted, and their extents released when
//! nothing else holds them, or their references dropped when something does.
//! An item that runs into the range is cut short, one that runs out of it
//! starts again at its end, and one that runs through it is split in two,
//! adding a reference to the extent both halves name: the cut
//! [`crate::clone`] makes for a clone over data. The size does not move. A
//! range lying wholly in a hole changes nothing at all, not even the times.
//!
//! # Part of a sector
//!
//! A punch whose range starts or ends partway through a sector zeros that part,
//! and keeps the sector, as `btrfs_truncate_block` does. A preallocation
//! running past a file that ends partway through a sector rewrites that last
//! sector the same way, with the size kept or not. The sector is written as
//! the kernel's writeback writes it:
//!
//! - in a preallocated extent this file alone holds, where it lies, the part of
//!   the item over it becoming a regular item over the same extent, joined with
//!   a regular neighbour over that extent;
//! - anywhere else, to a new extent of one sector with a regular item of its
//!   own, the old item cut around it.
//!
//! A partial sector at the range's edge lying in a hole stays a hole, with one
//! exception the kernel shares: a file's own partial last sector is rewritten
//! by a preallocation running past it, whatever holds it. Past the size rounded
//! up to a sector a punch zeros nothing, since nothing reads there.
//!
//! # What it refuses
//!
//! - a file holding inline data, which has no item to cut and no room beside it
//!   for an extent until that data is moved;
//! - a volume without the no-holes feature, where a hole is recorded as an
//!   item, and an explicit hole item in a range being preallocated;
//! - a regular extent lying past the file's size in a range being preallocated,
//!   which the kernel replaces;
//! - zeroing part of a sector of a regular extent in a `nodatacow` file, which
//!   the kernel writes in place, and of a preallocated extent another
//!   reference may read, which the kernel copies;
//! - encrypted or otherwise encoded extents;
//! - splitting an extent whose references are kept as items of their own;
//! - any subvolume other than the top level;
//! - space no data block group holds in one piece: allocating a block group,
//!   or a shorter extent than asked for, is not written;
//! - a range past the largest file offset, and a length of zero.

use std::collections::{BTreeMap, BTreeSet};

use crate::chunk::{objectid, DiskKey};
use crate::clone::{counted_bytes, cut_range, extent_items, order, Cut, Edits};
use crate::cow_write::regular_extent_item;
use crate::error::{Error, Result};
use crate::fs::{file_extent as fe, Filesystem, EXTENT_DATA_KEY, EXTENT_PREALLOC, EXTENT_REGULAR};
use crate::inode::{offsets as io, Inode, INODE_ITEM_KEY, INODE_NODATACOW, INODE_NODATASUM};
use crate::leaf_edit::OwnedItem;
use crate::superblock::le64;
use crate::transaction::{DataAdd, DataRelease, DataShare, DataWrite, ItemEdit};
use crate::write::window_inside_extent;

/// `BTRFS_FILE_EXTENT_INLINE`.
const INLINE: u8 = 0;
/// The longest extent one preallocation asks for, as the kernel's
/// `__btrfs_prealloc_file_range` limits it.
const MAX_PREALLOC_EXTENT: u64 = 256 << 20;
/// The largest file offset, the kernel's `MAX_LFS_FILESIZE`.
const LARGEST_OFFSET: u64 = i64::MAX as u64;
/// `BTRFS_INODE_PREALLOC`: the inode holds preallocated extents, which sends
/// the kernel's writes to where they lie.
const INODE_PREALLOC: u64 = 1 << 4;

/// A file's `EXTENT_DATA` items.
type ExtentItems = Vec<(DiskKey, Vec<u8>)>;

/// One sector rewritten, with part of it zeroed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rewrite {
    /// Where the sector starts.
    sector: u64,
    /// The bytes of it zeroed.
    zero: (u64, u64),
}

/// What a punch does: the range its items are cut over, which includes each
/// rewritten sector, and the sectors rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PunchPlan {
    cut: (u64, u64),
    rewrites: Vec<Rewrite>,
}

/// One change to a range of a file.
struct RangeChange {
    /// The range the file's items are cut over.
    cut: (u64, u64),
    /// The sectors rewritten, inside `cut`.
    rewrites: Vec<Rewrite>,
    /// The holes preallocated, each `(start, length)`.
    holes: Vec<(u64, u64)>,
    /// The size the file is left with.
    size: u64,
}

impl Filesystem {
    /// Give each hole of `[offset, offset + len)`, rounded out to whole
    /// sectors, a preallocated extent, as `fallocate(2)` does, as one committed
    /// transaction. Unless `keep_size` is set, the size grows to the range's
    /// end. The change and modification times move even when nothing else
    /// does.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but a regular file, and
    /// [`Error::UnsupportedFeature`] for every refusal the [module
    /// documentation](crate::fallocate) lists. Nothing is written unless
    /// everything is.
    pub fn fallocate(&mut self, ino: u64, offset: u64, len: u64, keep_size: bool) -> Result<()> {
        let (inode, items) = self.range_to_change(ino, offset, len)?;
        let sector = u64::from(self.sb.sectorsize);
        let end = offset + len;
        // Past a file ending partway through a sector, the rest of that sector
        // is rewritten as zeros first, whether or not the size then moves.
        let rewrites = if end > inode.size && !inode.size.is_multiple_of(sector) {
            let start = inode.size - inode.size % sector;
            vec![Rewrite {
                sector: start,
                zero: (inode.size, start + sector),
            }]
        } else {
            Vec::new()
        };
        let cut = rewrites
            .first()
            .map_or((0, 0), |r| (r.sector, r.sector + sector));
        let holes = holes_to_allocate(
            ino,
            &items,
            inode.size,
            (offset - offset % sector, end.next_multiple_of(sector)),
            cut,
        )?;
        // More than the volume holds is never asked of the allocator, so a
        // length the range check let through cannot ask for pieces by the
        // billion.
        let wanted: u128 = holes.iter().map(|&(_, len)| u128::from(len)).sum();
        if wanted > u128::from(self.sb.total_bytes) {
            return Err(refused(format!(
                "inode {ino}: the range's holes come to {wanted} bytes, more than the volume's \
                 {}",
                self.sb.total_bytes
            )));
        }
        let size = if keep_size {
            inode.size
        } else {
            inode.size.max(end)
        };
        self.change_range(
            &inode,
            &items,
            RangeChange {
                cut,
                rewrites,
                holes,
                size,
            },
        )
    }

    /// Take `[offset, offset + len)` out of a file, as `fallocate(2)` does
    /// with `FALLOC_FL_PUNCH_HOLE`, as one committed transaction. Whole sectors
    /// become a hole, part of a sector is zeroed, and the size does not move.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::fallocate`].
    pub fn punch_hole(&mut self, ino: u64, offset: u64, len: u64) -> Result<()> {
        let (inode, items) = self.range_to_change(ino, offset, len)?;
        let sector = u64::from(self.sb.sectorsize);
        let Some(plan) = plan_punch(&items, inode.size, sector, offset, offset + len) else {
            return Ok(());
        };
        self.change_range(
            &inode,
            &items,
            RangeChange {
                cut: plan.cut,
                rewrites: plan.rewrites,
                holes: Vec::new(),
                size: inode.size,
            },
        )
    }

    /// The inode and extent items of a file whose range may be changed.
    fn range_to_change(&self, ino: u64, offset: u64, len: u64) -> Result<(Inode, ExtentItems)> {
        if self.subvolume_writes.is_some() {
            return Err(refused(format!(
                "inode {ino} is in a subvolume other than the top level, and a range of a file \
                 is only preallocated or punched in the top level"
            )));
        }
        self.require_writable_top_level()?;
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() {
            return Err(Error::NotAFile);
        }
        crate::verity::refuse_mutation(ino, inode.flags)?;
        if len == 0 {
            return Err(refused("a length of zero".into()));
        }
        if offset
            .checked_add(len)
            .is_none_or(|end| end > LARGEST_OFFSET)
        {
            return Err(refused(format!(
                "{len} bytes at {offset} run past the largest file offset"
            )));
        }
        if !self.sb.has_no_holes() {
            return Err(refused(
                "a range changed on a volume without the no-holes feature records its holes as \
                 items, which is not written yet"
                    .into(),
            ));
        }
        let items = extent_items(self.items_of(ino)?);
        for (k, d) in &items {
            if d.get(fe::TYPE) == Some(&INLINE) {
                return Err(refused(format!(
                    "inode {ino} holds inline data, and changing a range of it means moving \
                     that data into an extent, which is not written yet"
                )));
            }
            if d.len() < fe::REGULAR_SIZE || !matches!(d[fe::TYPE], 1 | 2) {
                return Err(refused(format!(
                    "inode {ino}: the extent item at {} is of type {:?} and {} bytes, which \
                     changing a range does not handle",
                    k.offset,
                    d.get(fe::TYPE),
                    d.len()
                )));
            }
            if d[fe::ENCRYPTION] != 0 || d[fe::OTHER_ENCODING..fe::OTHER_ENCODING + 2] != [0, 0] {
                return Err(refused(format!(
                    "inode {ino}: the extent item at {} is encrypted or otherwise encoded",
                    k.offset
                )));
            }
        }
        Ok((inode, items))
    }

    /// Make `change` to the file, as one committed transaction.
    fn change_range(
        &mut self,
        inode: &Inode,
        items: &[(DiskKey, Vec<u8>)],
        change: RangeChange,
    ) -> Result<()> {
        let ino = inode.ino;
        let generation = self.next_generation()?;
        // An empty range cuts nothing: it would split the item holding it.
        let cut = if change.cut.0 < change.cut.1 {
            cut_range(ino, items, change.cut.0, change.cut.1)?
        } else {
            Cut::default()
        };
        let mut covered = cut.covered;
        let mut edits = Edits::new();
        for edit in cut.edits {
            edits.insert(order(&edit.key()), edit);
        }
        for (k, _) in &covered {
            edits.insert(order(k), ItemEdit::Delete(*k));
        }
        let mut shares = Vec::new();
        for (bytenr, ref_offset) in cut.splits {
            shares.push(self.one_more_reference(bytenr, ref_offset)?);
        }

        let mut new_sectors = Vec::new();
        let mut written: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut in_place = Vec::new();
        for rewrite in &change.rewrites {
            let bytes = self.rewritten_bytes(inode, items, rewrite)?;
            let Some(part) = self.written_in_place(inode, items, rewrite, generation)? else {
                new_sectors.push((rewrite.sector, bytes));
                continue;
            };
            edits.insert(
                (ino, EXTENT_DATA_KEY, rewrite.sector),
                ItemEdit::Put(OwnedItem {
                    key: DiskKey {
                        objectid: ino,
                        key_type: EXTENT_DATA_KEY,
                        offset: rewrite.sector,
                    },
                    data: part.item,
                }),
            );
            written.push((part.at, bytes));
            in_place.push((part.bytenr, rewrite.sector));
        }
        // The kernel joins a sector written where it lies with a regular
        // neighbour over the same extent. Such an extent is this file's alone,
        // so its references are counted again, item by item.
        let extents: BTreeSet<u64> = in_place.iter().map(|&(bytenr, _)| bytenr).collect();
        let (mut releases, mut unrefs) = (Vec::new(), Vec::new());
        if !extents.is_empty() {
            let sectors: Vec<u64> = in_place.iter().map(|&(_, at)| at).collect();
            join_written_sectors(items, &mut edits, &sectors, generation);
            shares.retain(|s| !extents.contains(&s.bytenr));
            covered.retain(|(_, d)| !extents.contains(&le64(d, fe::DISK_BYTENR)));
            for ((bytenr, ref_offset), change) in reference_changes(items, &edits, &extents) {
                let (_, old_len, _) = self.extent_item(bytenr)?;
                for _ in 0..change.unsigned_abs() {
                    if change > 0 {
                        shares.push(self.one_more_reference(bytenr, ref_offset)?);
                    } else {
                        unrefs.push(DataRelease {
                            old: bytenr,
                            old_len,
                            old_ref_offset: ref_offset,
                        });
                    }
                }
            }
        }
        let (dropped, kept) = self.drop_covered(ino, &covered)?;
        releases.extend(dropped);
        unrefs.extend(kept);

        // Everything allocated: each sector rewritten to a new extent, then
        // each hole, a piece at a time.
        let pieces: Vec<(u64, u64)> = change
            .holes
            .iter()
            .flat_map(|&(start, len)| prealloc_pieces(start, len))
            .collect();
        let lens: Vec<u64> = new_sectors
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .chain(pieces.iter().map(|&(_, len)| len))
            .collect();
        let news = self.find_data_extents(&lens)?;
        let (sector_news, hole_news) = news.split_at(new_sectors.len());
        let mut adds = Vec::with_capacity(news.len());
        let mut put = |edits: &mut Edits, file_offset: u64, new: u64, len: u64, kind: u8| {
            let mut data = regular_extent_item(generation, new, len, len, 0);
            data[fe::TYPE] = kind;
            let key = DiskKey {
                objectid: ino,
                key_type: EXTENT_DATA_KEY,
                offset: file_offset,
            };
            edits.insert(order(&key), ItemEdit::Put(OwnedItem { key, data }));
            adds.push(DataAdd {
                new,
                disk_len: len,
                file_offset,
            });
        };
        for ((file_offset, bytes), &new) in new_sectors.into_iter().zip(sector_news) {
            put(
                &mut edits,
                file_offset,
                new,
                bytes.len() as u64,
                EXTENT_REGULAR,
            );
            written.push((new, bytes));
        }
        for (&(file_offset, len), &new) in pieces.iter().zip(hole_news) {
            put(&mut edits, file_offset, new, len, EXTENT_PREALLOC);
        }

        let nbytes = counted_bytes(items, &edits)?;
        let released: Vec<(u64, u64)> = releases
            .iter()
            .map(|r| (r.old, r.old + r.old_len))
            .collect();
        let summed = inode.flags & INODE_NODATASUM == 0;
        let csum_edits = if released.is_empty() && written.is_empty() {
            Vec::new()
        } else {
            self.csum_edits(&released, &written, summed)?
        };

        let now = crate::namespace::now();
        let mut raw = self.raw_inode(ino)?;
        raw[io::SIZE..io::SIZE + 8].copy_from_slice(&change.size.to_le_bytes());
        raw[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
        if !pieces.is_empty() {
            let flags = inode.flags | INODE_PREALLOC;
            raw[io::FLAGS..io::FLAGS + 8].copy_from_slice(&flags.to_le_bytes());
        }
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME, io::MTIME]);
        let mut edits: Vec<ItemEdit> = edits.into_values().collect();
        edits.push(ItemEdit::Put(OwnedItem {
            key: DiskKey {
                objectid: ino,
                key_type: INODE_ITEM_KEY,
                offset: 0,
            },
            data: raw,
        }));

        // The data, to every mirror, before any tree block: until the commit
        // lands nothing names a new extent, and a preallocated item still
        // reads as zeros.
        for (at, bytes) in &written {
            self.mirror_spans(*at, bytes.len())?;
        }
        for (at, bytes) in &written {
            self.write_logical_all_mirrors(*at, bytes)?;
        }
        let write = DataWrite {
            root: objectid::FS_TREE,
            ino,
            adds,
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

    /// A rewritten sector's bytes: what it reads now, with its part zeroed.
    /// Past the end of the file it reads as zeros.
    fn rewritten_bytes(
        &self,
        inode: &Inode,
        items: &[(DiskKey, Vec<u8>)],
        rewrite: &Rewrite,
    ) -> Result<Vec<u8>> {
        let sector = u64::from(self.sb.sectorsize);
        let holder = holding_item(items, rewrite.sector);
        let holds_regular = holder.is_some_and(|(_, d)| d[fe::TYPE] == EXTENT_REGULAR);
        if holds_regular && inode.flags & INODE_NODATACOW != 0 {
            return Err(refused(format!(
                "inode {} is nodatacow, and zeroing part of its sector at {} in place is not \
                 written yet",
                inode.ino, rewrite.sector
            )));
        }
        let mut bytes = vec![0u8; sector as usize];
        self.read_at(inode.ino, rewrite.sector, &mut bytes)?;
        let from = (rewrite.zero.0 - rewrite.sector) as usize;
        let to = (rewrite.zero.1 - rewrite.sector) as usize;
        bytes[from..to].fill(0);
        Ok(bytes)
    }

    /// How `rewrite`'s sector is written where it lies, when a preallocated
    /// extent holds it: `None` for a sector anything else holds.
    fn written_in_place(
        &self,
        inode: &Inode,
        items: &[(DiskKey, Vec<u8>)],
        rewrite: &Rewrite,
        generation: u64,
    ) -> Result<Option<InPlace>> {
        let ino = inode.ino;
        let Some((k, d)) = holding_item(items, rewrite.sector) else {
            return Ok(None);
        };
        if d[fe::TYPE] != EXTENT_PREALLOC {
            return Ok(None);
        }
        let bytenr = le64(d, fe::DISK_BYTENR);
        let within = le64(d, fe::OFFSET);
        let sector = u64::from(self.sb.sectorsize);
        let at = bytenr + within + (rewrite.sector - k.offset);
        let (refs, extent_len, made) = self.extent_item(bytenr)?;
        if !window_inside_extent(at, sector, bytenr, extent_len) {
            return Err(refused(format!(
                "inode {ino}: offset {} maps to [{at}, +{sector}), outside the {extent_len}-byte \
                 extent the extent tree records at {bytenr}",
                rewrite.sector
            )));
        }
        let ours = items
            .iter()
            .filter(|(_, d)| le64(d, fe::DISK_BYTENR) == bytenr)
            .count() as u64;
        let last_snapshot = self.root_last_snapshot(objectid::FS_TREE)?;
        if refs != ours || made <= last_snapshot || self.leaf_shared(objectid::FS_TREE, k)? {
            return Err(refused(format!(
                "inode {ino}: the preallocated extent at {bytenr} has {refs} references to this \
                 file's {ours} items or predates the snapshot taken at {last_snapshot}, and \
                 zeroing part of a sector another reference may read is not written yet"
            )));
        }
        let mut item = d.clone();
        item[fe::GENERATION..fe::GENERATION + 8].copy_from_slice(&generation.to_le_bytes());
        item[fe::TYPE] = EXTENT_REGULAR;
        let into = within + (rewrite.sector - k.offset);
        item[fe::OFFSET..fe::OFFSET + 8].copy_from_slice(&into.to_le_bytes());
        item[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&sector.to_le_bytes());
        Ok(Some(InPlace { bytenr, at, item }))
    }

    /// One more reference to the extent at `bytenr`, at `ref_offset`.
    fn one_more_reference(&self, bytenr: u64, ref_offset: u64) -> Result<DataShare> {
        let (_, len, _) = self.extent_item(bytenr)?;
        self.refuse_keyed_data_refs(bytenr)?;
        Ok(DataShare {
            bytenr,
            len,
            ref_offset,
        })
    }
}

/// A sector written where it lies in a preallocated extent.
struct InPlace {
    /// The extent.
    bytenr: u64,
    /// Where the sector lies.
    at: u64,
    /// The regular item over it.
    item: Vec<u8>,
}

/// The item holding the byte at `at` in an extent, if any: a hole holds nothing.
fn holding_item(items: &[(DiskKey, Vec<u8>)], at: u64) -> Option<&(DiskKey, Vec<u8>)> {
    items.iter().find(|(k, d)| {
        le64(d, fe::DISK_BYTENR) != 0
            && k.offset <= at
            && at < k.offset.saturating_add(le64(d, fe::NUM_BYTES))
    })
}

/// The file's items by key offset, once `edits` apply.
fn items_after(items: &[(DiskKey, Vec<u8>)], edits: &Edits) -> BTreeMap<u64, Vec<u8>> {
    let mut after: BTreeMap<u64, Vec<u8>> =
        items.iter().map(|(k, d)| (k.offset, d.clone())).collect();
    for edit in edits.values() {
        match edit {
            ItemEdit::Delete(k) => {
                after.remove(&k.offset);
            }
            ItemEdit::Put(item) => {
                after.insert(item.key.offset, item.data.clone());
            }
        }
    }
    after
}

/// Whether the item at `at` joins the one before it, ending at `at`, as the
/// kernel's `extent_mergeable` decides it: both regular and unencoded, over one
/// extent at one reference offset.
fn joinable(before_at: u64, before: &[u8], at: u64, item: &[u8]) -> bool {
    let plain = |d: &[u8]| {
        d[fe::TYPE] == EXTENT_REGULAR
            && d[fe::COMPRESSION] == 0
            && d[fe::ENCRYPTION] == 0
            && d[fe::OTHER_ENCODING..fe::OTHER_ENCODING + 2] == [0, 0]
    };
    plain(before)
        && plain(item)
        && before_at + le64(before, fe::NUM_BYTES) == at
        && le64(before, fe::DISK_BYTENR) == le64(item, fe::DISK_BYTENR)
        && before_at.wrapping_sub(le64(before, fe::OFFSET))
            == at.wrapping_sub(le64(item, fe::OFFSET))
}

/// Join each of `sectors`, written where they lie, with a regular neighbour
/// over the same extent, as `btrfs_mark_extent_written` does.
fn join_written_sectors(
    items: &[(DiskKey, Vec<u8>)],
    edits: &mut Edits,
    sectors: &[u64],
    generation: u64,
) {
    let ino = items.first().map_or(0, |(k, _)| k.objectid);
    let existing: BTreeSet<u64> = items.iter().map(|(k, _)| k.offset).collect();
    let mut after = items_after(items, edits);
    let key = |offset: u64| DiskKey {
        objectid: ino,
        key_type: EXTENT_DATA_KEY,
        offset,
    };
    for &sector in sectors {
        let Some(&start) = after.range(..=sector).next_back().map(|(at, _)| at) else {
            continue;
        };
        // The neighbour after it, then the one before.
        let next = after.range(start + 1..).next().map(|(at, _)| *at);
        let prev = after.range(..start).next_back().map(|(at, _)| *at);
        for (first, second) in [(Some(start), next), (prev, Some(start))] {
            let (Some(first), Some(second)) = (first, second) else {
                continue;
            };
            let (Some(a), Some(b)) = (after.get(&first), after.get(&second)) else {
                continue;
            };
            if !joinable(first, a, second, b) {
                continue;
            }
            let mut joined = a.clone();
            let num = le64(a, fe::NUM_BYTES) + le64(b, fe::NUM_BYTES);
            joined[fe::NUM_BYTES..fe::NUM_BYTES + 8].copy_from_slice(&num.to_le_bytes());
            joined[fe::GENERATION..fe::GENERATION + 8].copy_from_slice(&generation.to_le_bytes());
            after.remove(&second);
            after.insert(first, joined.clone());
            if existing.contains(&second) {
                edits.insert(order(&key(second)), ItemEdit::Delete(key(second)));
            } else {
                edits.remove(&order(&key(second)));
            }
            edits.insert(
                order(&key(first)),
                ItemEdit::Put(OwnedItem {
                    key: key(first),
                    data: joined,
                }),
            );
        }
    }
}

/// How far `edits` move each reference to `extents`, by address and reference
/// offset.
fn reference_changes(
    items: &[(DiskKey, Vec<u8>)],
    edits: &Edits,
    extents: &BTreeSet<u64>,
) -> BTreeMap<(u64, u64), i64> {
    let mut changes: BTreeMap<(u64, u64), i64> = BTreeMap::new();
    let mut count = |at: u64, d: &[u8], by: i64| {
        let bytenr = le64(d, fe::DISK_BYTENR);
        if extents.contains(&bytenr) {
            let reference = at.wrapping_sub(le64(d, fe::OFFSET));
            *changes.entry((bytenr, reference)).or_default() += by;
        }
    };
    for (k, d) in items {
        count(k.offset, d, -1);
    }
    for (at, d) in &items_after(items, edits) {
        count(*at, d, 1);
    }
    changes.retain(|_, change| *change != 0);
    changes
}

/// What a punch of `[offset, end)` does, as `btrfs_punch_hole` decides it, or
/// `None` when the range holds no extent, and nothing changes.
///
/// A range that starts in a hole is taken to start at the first extent past
/// it, which begins on a sector. Part of a sector is zeroed where the range
/// starts or ends inside one that holds an extent and that lies inside the
/// size rounded up to a sector.
fn plan_punch(
    items: &[(DiskKey, Vec<u8>)],
    size: u64,
    sector: u64,
    offset: u64,
    end: u64,
) -> Option<PunchPlan> {
    let start = items
        .iter()
        .filter(|(_, d)| le64(d, fe::DISK_BYTENR) != 0)
        .map(|(k, d)| (k.offset, k.offset.saturating_add(le64(d, fe::NUM_BYTES))))
        .filter(|&(_, stop)| stop > offset)
        .map(|(from, _)| from.max(offset))
        .min()
        .filter(|&start| start < end)?;
    let rounded = size.next_multiple_of(sector);
    let first = start - start % sector;
    if first == (end - 1) - (end - 1) % sector && end - start < sector {
        if start >= rounded {
            return Some(PunchPlan {
                cut: (first, first),
                rewrites: Vec::new(),
            });
        }
        return Some(PunchPlan {
            cut: (first, first + sector),
            rewrites: vec![Rewrite {
                sector: first,
                zero: (start, end),
            }],
        });
    }
    let mut cut = (start.next_multiple_of(sector), end - end % sector);
    let mut rewrites = Vec::new();
    if first != start && start < rounded {
        rewrites.push(Rewrite {
            sector: first,
            zero: (start, first + sector),
        });
        cut.0 = first;
    }
    if !end.is_multiple_of(sector) && end < rounded && holding_item(items, cut.1).is_some() {
        rewrites.push(Rewrite {
            sector: cut.1,
            zero: (cut.1, end),
        });
        cut.1 += sector;
    }
    Some(PunchPlan { cut, rewrites })
}

/// The holes of `[from, to)` a preallocation fills, each `(start, length)`,
/// as `btrfs_fallocate` finds them: what no item holds, less the range
/// `rewritten`, which the same change gives an extent.
fn holes_to_allocate(
    ino: u64,
    items: &[(DiskKey, Vec<u8>)],
    size: u64,
    (from, to): (u64, u64),
    rewritten: (u64, u64),
) -> Result<Vec<(u64, u64)>> {
    let mut held: Vec<(u64, u64)> = Vec::with_capacity(items.len() + 1);
    for (k, d) in items {
        let stop = k.offset.saturating_add(le64(d, fe::NUM_BYTES));
        if stop <= from || k.offset >= to {
            continue;
        }
        if le64(d, fe::DISK_BYTENR) == 0 {
            return Err(refused(format!(
                "inode {ino}: the range holds an explicit hole item at {}, which is not \
                 preallocated over yet",
                k.offset
            )));
        }
        if d[fe::TYPE] == EXTENT_REGULAR && k.offset >= size {
            return Err(refused(format!(
                "inode {ino}: the regular extent item at {} lies past the file's size at {size}, \
                 and replacing it is not written yet",
                k.offset
            )));
        }
        held.push((k.offset, stop));
    }
    if rewritten.0 < rewritten.1 {
        held.push(rewritten);
    }
    held.sort_unstable();
    let mut holes = Vec::new();
    let mut at = from;
    for (start, stop) in held {
        if start > at && at < to {
            holes.push((at, start.min(to) - at));
        }
        at = at.max(stop);
    }
    if at < to {
        holes.push((at, to - at));
    }
    Ok(holes)
}

/// A hole's extents, each `(file offset, length)`.
fn prealloc_pieces(start: u64, len: u64) -> impl Iterator<Item = (u64, u64)> {
    (0..len.div_ceil(MAX_PREALLOC_EXTENT)).map(move |n| {
        let at = n * MAX_PREALLOC_EXTENT;
        (start + at, MAX_PREALLOC_EXTENT.min(len - at))
    })
}

fn refused(why: String) -> Error {
    Error::UnsupportedFeature(why)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 4096;

    fn item(offset: u64, kind: u8, bytenr: u64, num: u64) -> (DiskKey, Vec<u8>) {
        let mut d = vec![0u8; fe::REGULAR_SIZE];
        d[fe::TYPE] = kind;
        d[fe::DISK_BYTENR..fe::DISK_BYTENR + 8].copy_from_slice(&bytenr.to_le_bytes());
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

    fn data(offset: u64, num: u64) -> (DiskKey, Vec<u8>) {
        item(offset, EXTENT_REGULAR, 1 << 20, num)
    }

    #[test]
    fn a_punch_inside_a_hole_changes_nothing() {
        let items = vec![data(0, 2 * S), data(8 * S, 2 * S)];
        assert_eq!(plan_punch(&items, 10 * S, S, 2 * S, 8 * S), None);
        assert_eq!(plan_punch(&items, 10 * S, S, 3 * S + 7, 5), None);
        assert_eq!(plan_punch(&[], 0, S, 0, S), None);
    }

    #[test]
    fn a_punch_of_whole_sectors_rewrites_nothing() {
        let items = vec![data(0, 16 * S)];
        assert_eq!(
            plan_punch(&items, 16 * S, S, 4 * S, 8 * S),
            Some(PunchPlan {
                cut: (4 * S, 8 * S),
                rewrites: Vec::new(),
            })
        );
    }

    #[test]
    fn a_punch_off_sectors_rewrites_the_sector_at_each_end() {
        let items = vec![data(0, 16 * S)];
        assert_eq!(
            plan_punch(&items, 16 * S, S, 1024, 1024 + 2 * S),
            Some(PunchPlan {
                cut: (0, 3 * S),
                rewrites: vec![
                    Rewrite {
                        sector: 0,
                        zero: (1024, S),
                    },
                    Rewrite {
                        sector: 2 * S,
                        zero: (2 * S, 2 * S + 1024),
                    },
                ],
            })
        );
    }

    #[test]
    fn a_punch_inside_one_sector_rewrites_that_sector_alone() {
        let items = vec![data(0, 16 * S)];
        assert_eq!(
            plan_punch(&items, 16 * S, S, 100, 300),
            Some(PunchPlan {
                cut: (0, S),
                rewrites: vec![Rewrite {
                    sector: 0,
                    zero: (100, 300),
                }],
            })
        );
    }

    #[test]
    fn a_punch_starting_in_a_hole_starts_at_the_next_extent() {
        let items = vec![data(S, 3 * S)];
        assert_eq!(
            plan_punch(&items, 4 * S, S, 100, 2 * S),
            Some(PunchPlan {
                cut: (S, 2 * S),
                rewrites: Vec::new(),
            })
        );
    }

    #[test]
    fn a_punch_ending_in_a_hole_or_past_the_size_leaves_that_sector_alone() {
        // The range's last sector is a hole.
        let items = vec![data(0, S)];
        assert_eq!(
            plan_punch(&items, 4 * S, S, 0, S + 100),
            Some(PunchPlan {
                cut: (0, S),
                rewrites: Vec::new(),
            })
        );
        // Its last sector holds a kept item past the size, rounded up.
        let items = vec![data(0, S), item(S, EXTENT_PREALLOC, 2 << 20, 4 * S)];
        assert_eq!(
            plan_punch(&items, S, S, 2 * S, 3 * S + 100),
            Some(PunchPlan {
                cut: (2 * S, 3 * S),
                rewrites: Vec::new(),
            })
        );
    }

    #[test]
    fn a_punch_to_the_end_of_a_partial_last_sector_zeros_its_front() {
        let items = vec![data(0, 2 * S)];
        assert_eq!(
            plan_punch(&items, S + 904, S, 0, S + 904),
            Some(PunchPlan {
                cut: (0, 2 * S),
                rewrites: vec![Rewrite {
                    sector: S,
                    zero: (S, S + 904),
                }],
            })
        );
    }

    #[test]
    fn holes_are_what_no_item_holds() {
        let items = vec![data(0, 2 * S), data(8 * S, 2 * S)];
        assert_eq!(
            holes_to_allocate(257, &items, 10 * S, (0, 16 * S), (0, 0)).unwrap(),
            vec![(2 * S, 6 * S), (10 * S, 6 * S)]
        );
        assert_eq!(
            holes_to_allocate(257, &items, 10 * S, (0, 2 * S), (0, 0)).unwrap(),
            Vec::new()
        );
        // A range starting and ending in holes is cut to them.
        assert_eq!(
            holes_to_allocate(257, &items, 10 * S, (3 * S, 9 * S), (0, 0)).unwrap(),
            vec![(3 * S, 5 * S)]
        );
        // A sector the same change rewrites holds its range.
        assert_eq!(
            holes_to_allocate(257, &[], S + 1, (0, 4 * S), (S, 2 * S)).unwrap(),
            vec![(0, S), (2 * S, 2 * S)]
        );
    }

    #[test]
    fn a_regular_extent_past_the_size_and_a_hole_item_are_refused() {
        let items = vec![data(4 * S, S)];
        let err = holes_to_allocate(257, &items, S, (0, 8 * S), (0, 0)).unwrap_err();
        assert!(err.to_string().contains("past the file's size"), "{err}");
        let items = vec![item(0, EXTENT_REGULAR, 0, S)];
        let err = holes_to_allocate(257, &items, S, (0, 8 * S), (0, 0)).unwrap_err();
        assert!(err.to_string().contains("explicit hole"), "{err}");
    }

    /// A regular or preallocated item over the extent at 1 MiB, its window
    /// starting where the file offset does.
    fn window(offset: u64, kind: u8, num: u64) -> (DiskKey, Vec<u8>) {
        let (k, mut d) = item(offset, kind, 1 << 20, num);
        d[fe::OFFSET..fe::OFFSET + 8].copy_from_slice(&offset.to_le_bytes());
        (k, d)
    }

    fn put((key, data): (DiskKey, Vec<u8>)) -> ItemEdit {
        ItemEdit::Put(OwnedItem { key, data })
    }

    #[test]
    fn a_sector_written_in_place_joins_its_regular_neighbours() {
        let items = vec![
            window(0, EXTENT_REGULAR, S),
            window(S, EXTENT_PREALLOC, S),
            window(2 * S, EXTENT_REGULAR, S),
            window(3 * S, EXTENT_PREALLOC, S),
        ];
        let written = put(window(S, EXTENT_REGULAR, S));
        let mut edits: Edits = [(order(&written.key()), written)].into_iter().collect();
        join_written_sectors(&items, &mut edits, &[S], 7);
        let after = items_after(&items, &edits);
        assert_eq!(after.keys().copied().collect::<Vec<_>>(), vec![0, 3 * S]);
        assert_eq!(le64(&after[&0], fe::NUM_BYTES), 3 * S);
        assert_eq!(le64(&after[&0], fe::GENERATION), 7);
        assert_eq!(after[&(3 * S)][fe::TYPE], EXTENT_PREALLOC);
        // Three items became one, and the extent's record counts two fewer.
        let extents = BTreeSet::from([1 << 20]);
        assert_eq!(
            reference_changes(&items, &edits, &extents),
            BTreeMap::from([((1 << 20, 0), -2)])
        );
    }

    #[test]
    fn a_sector_written_in_place_stays_apart_from_what_it_cannot_join() {
        // A hole between; another extent; another reference offset.
        let mut other_extent = window(2 * S, EXTENT_REGULAR, S);
        other_extent.1[fe::DISK_BYTENR..fe::DISK_BYTENR + 8]
            .copy_from_slice(&(2u64 << 20).to_le_bytes());
        let mut other_reference = window(0, EXTENT_REGULAR, S);
        other_reference.1[fe::OFFSET..fe::OFFSET + 8].copy_from_slice(&S.to_le_bytes());
        let far = window(3 * S, EXTENT_REGULAR, S);
        for neighbour in [other_extent, other_reference, far] {
            let items = vec![neighbour.clone(), window(S, EXTENT_PREALLOC, S)];
            let written = put(window(S, EXTENT_REGULAR, S));
            let mut edits: Edits = [(order(&written.key()), written)].into_iter().collect();
            join_written_sectors(&items, &mut edits, &[S], 7);
            assert_eq!(items_after(&items, &edits).len(), 2);
        }
    }

    #[test]
    fn a_long_hole_is_allocated_a_piece_at_a_time() {
        let pieces: Vec<_> = prealloc_pieces(S, MAX_PREALLOC_EXTENT + S).collect();
        assert_eq!(
            pieces,
            vec![(S, MAX_PREALLOC_EXTENT), (S + MAX_PREALLOC_EXTENT, S)]
        );
        assert_eq!(prealloc_pieces(0, S).collect::<Vec<_>>(), vec![(0, S)]);
    }
}
