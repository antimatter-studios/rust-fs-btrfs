//! Files as a filesystem tree still describes them: inodes, the names
//! that claim them, their extents, and the bytes behind each extent.
//!
//! The mount resolves a file by path and refuses at the first damaged
//! structure. Here a whole filesystem tree is walked past its damage and
//! every inode is gathered with every claim made about it, from verified
//! leaves and damaged ones alike, so a caller can rebuild the namespace
//! and read each file by its extents without trusting any one path.

use super::csums::{verify_data, CsumTree, SectorTally};
use super::tree::{walk_tree, Layout, RootPointer, WalkReport};
use crate::compression::{decompress, Compression, ShortDecode};
use crate::dir::{parse_dir_items, DIR_INDEX_KEY};
use crate::fs::EXTENT_DATA_KEY;
use crate::inode::{Inode, INODE_ITEM_KEY, INODE_REF_KEY};
use crate::superblock::{le16, le64};
use fs_core::BlockRead;
use std::collections::BTreeMap;
use std::io;
use std::ops::ControlFlow;

/// `btrfs_file_extent_item.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtentType {
    /// The data is inside the item.
    Inline,
    /// The data is in an extent on disk.
    Regular,
    /// Space reserved on disk that reads as zeros.
    Prealloc,
}

/// One `EXTENT_DATA` item: a run of a file and where its bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileExtentItem {
    /// Where the run starts in the file: the key's offset.
    pub file_offset: u64,
    pub generation: u64,
    /// What the whole extent decodes to.
    pub ram_bytes: u64,
    /// The raw compression byte: 0 none, 1 zlib, 2 LZO, 3 zstd.
    pub compression: u8,
    pub encryption: u8,
    pub other_encoding: u16,
    pub kind: ExtentType,
    /// An inline extent's stored bytes, still compressed if `compression`
    /// says so.
    pub inline: Vec<u8>,
    /// The extent's logical start on disk; 0 for a hole.
    pub disk_bytenr: u64,
    /// Bytes the extent occupies on disk.
    pub disk_num_bytes: u64,
    /// Where this reference starts within the extent's decoded bytes.
    pub offset: u64,
    /// How many of the file's bytes this reference covers.
    pub num_bytes: u64,
}

impl FileExtentItem {
    /// Decode an `EXTENT_DATA` item filed at `file_offset`. `None` when the
    /// item is too short for its type or names no type the format defines.
    pub fn parse(file_offset: u64, data: &[u8]) -> Option<Self> {
        const INLINE_DATA: usize = 21;
        const REGULAR_SIZE: usize = 53;
        if data.len() < INLINE_DATA {
            return None;
        }
        let kind = match data[20] {
            0 => ExtentType::Inline,
            1 => ExtentType::Regular,
            2 => ExtentType::Prealloc,
            _ => return None,
        };
        let mut item = Self {
            file_offset,
            generation: le64(data, 0),
            ram_bytes: le64(data, 8),
            compression: data[16],
            encryption: data[17],
            other_encoding: le16(data, 18),
            kind,
            inline: Vec::new(),
            disk_bytenr: 0,
            disk_num_bytes: 0,
            offset: 0,
            num_bytes: 0,
        };
        if kind == ExtentType::Inline {
            item.inline = data[INLINE_DATA..].to_vec();
            item.num_bytes = item.ram_bytes;
            return Some(item);
        }
        if data.len() < REGULAR_SIZE {
            return None;
        }
        item.disk_bytenr = le64(data, 21);
        item.disk_num_bytes = le64(data, 29);
        item.offset = le64(data, 37);
        item.num_bytes = le64(data, 45);
        Some(item)
    }

    /// A regular extent with no bytes on disk.
    pub fn is_hole(&self) -> bool {
        self.kind == ExtentType::Regular && self.disk_bytenr == 0
    }

    /// The source byte ranges that hold this extent, as `(logical,
    /// length)`: the whole compressed run for a compressed extent, the
    /// referenced window otherwise. Empty for inline data, holes and
    /// preallocation.
    pub fn logical_range(&self) -> Option<(u64, u64)> {
        if self.kind != ExtentType::Regular || self.disk_bytenr == 0 {
            return None;
        }
        if self.compression != 0 {
            Some((self.disk_bytenr, self.disk_num_bytes))
        } else {
            Some((self.disk_bytenr + self.offset, self.num_bytes))
        }
    }
}

/// Why some of an extent's bytes are not what was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataFault {
    /// No copy of these logical bytes could be read. They are zeros in
    /// the output.
    Unreadable { logical: u64, length: u64 },
    /// The extent's bytes were read but did not decompress. The output
    /// holds zeros for the whole reference.
    Decompress(String),
    /// An encoding this crate cannot undo: encryption or another encoding.
    Unsupported(String),
    /// These logical bytes were read, but their checksum matched on no
    /// copy. They are kept as read: damaged evidence, not zeros.
    Checksum { logical: u64, length: u64 },
}

/// Read `length` bytes at `logical`, trying each copy, sector by sector
/// for a range the first copy cannot give whole. Sectors no copy gives
/// are zeros, and named.
pub fn read_data(
    source: &dyn BlockRead,
    layout: &Layout,
    logical: u64,
    length: u64,
) -> (Vec<u8>, Vec<DataFault>) {
    let mut bytes = vec![0u8; length as usize];
    if layout.read_logical(source, logical, 0, &mut bytes).is_ok() {
        return (bytes, Vec::new());
    }
    let sector = u64::from(layout.sectorsize).max(512);
    let copies = layout.copies(logical).max(1);
    let mut faults: Vec<DataFault> = Vec::new();
    let mut at = 0u64;
    while at < length {
        let take = (sector - (logical + at) % sector).min(length - at);
        let window = &mut bytes[at as usize..(at + take) as usize];
        let read = (0..copies).any(|mirror| {
            layout
                .read_logical(source, logical + at, mirror, window)
                .is_ok()
        });
        if !read {
            window.fill(0);
            match faults.last_mut() {
                Some(DataFault::Unreadable {
                    logical: start,
                    length: run,
                }) if *start + *run == logical + at => *run += take,
                _ => faults.push(DataFault::Unreadable {
                    logical: logical + at,
                    length: take,
                }),
            }
        }
        at += take;
    }
    (bytes, faults)
}

/// The bytes one extent item contributes to its file: `num_bytes` of them,
/// decoded, zeros for holes and preallocation, and zeros where the data
/// could not be read or decoded, with the reason.
///
/// `last` says the item is the file's last extent, where a compressed
/// stream may legitimately decode short of `ram_bytes`.
pub fn read_file_extent(
    source: &dyn BlockRead,
    layout: &Layout,
    csums: Option<&CsumTree>,
    item: &FileExtentItem,
    last: bool,
) -> (Vec<u8>, Vec<DataFault>, SectorTally) {
    let none = SectorTally::default();
    let length = item.num_bytes as usize;
    if item.encryption != 0 || item.other_encoding != 0 {
        return (
            vec![0u8; length],
            vec![DataFault::Unsupported(format!(
                "encryption {} other encoding {}",
                item.encryption, item.other_encoding
            ))],
            none,
        );
    }
    let algo = match Compression::from_byte(item.compression) {
        Ok(algo) => algo,
        Err(error) => {
            return (
                vec![0u8; length],
                vec![DataFault::Unsupported(error.to_string())],
                none,
            )
        }
    };
    let short = if last {
        ShortDecode::Pad
    } else {
        ShortDecode::Refuse
    };
    let decode = |raw: &[u8], window: (u64, usize)| -> (Vec<u8>, Vec<DataFault>) {
        let (offset, take) = window;
        match decompress(
            algo,
            raw,
            item.ram_bytes as usize,
            layout.sectorsize as usize,
            short,
        ) {
            Ok(decoded) => {
                let start = (offset as usize).min(decoded.len());
                let end = start.saturating_add(take).min(decoded.len());
                let mut out = decoded[start..end].to_vec();
                out.resize(take, 0);
                (out, Vec::new())
            }
            Err(error) => (
                vec![0u8; take],
                vec![DataFault::Decompress(error.to_string())],
            ),
        }
    };
    match item.kind {
        ExtentType::Inline => {
            if algo == Compression::None {
                let mut out = item.inline.clone();
                out.resize(length, 0);
                (out, Vec::new(), none)
            } else {
                let (out, faults) = decode(&item.inline, (0, length));
                (out, faults, none)
            }
        }
        ExtentType::Prealloc => (vec![0u8; length], Vec::new(), none),
        ExtentType::Regular if item.disk_bytenr == 0 => (vec![0u8; length], Vec::new(), none),
        ExtentType::Regular => {
            let (logical, span) = item.logical_range().expect("a regular extent on disk");
            let (mut raw, mut faults) = read_data(source, layout, logical, span);
            let mut tally = none;
            if let Some(csums) = csums {
                let (checked, failed) = verify_data(source, layout, csums, logical, &mut raw);
                tally = checked;
                faults.extend(
                    failed
                        .into_iter()
                        .map(|(logical, length)| DataFault::Checksum { logical, length }),
                );
            } else {
                tally.unchecked = span.div_ceil(u64::from(layout.sectorsize));
            }
            if algo == Compression::None {
                return (raw, faults, tally);
            }
            let (out, mut more) = decode(&raw, (item.offset, length));
            faults.append(&mut more);
            (out, faults, tally)
        }
    }
}

/// A name that claims an inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameClaim {
    /// The directory the name is in.
    pub parent: u64,
    pub name: Vec<u8>,
    /// The directory index the name is filed at.
    pub index: u64,
    /// Whether the claim came from the parent's `DIR_INDEX` item (true) or
    /// from the inode's own `INODE_REF` (false).
    pub from_directory: bool,
    /// Whether the leaf holding the claim verified.
    pub verified: bool,
}

/// One extent item of a file, with the verdict of the leaf it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentClaim {
    pub item: FileExtentItem,
    pub verified: bool,
    /// The leaf the item is in, and the copy of it that was read.
    pub leaf: u64,
    pub leaf_mirror: usize,
    /// Where the item's data starts within the leaf, in bytes from the
    /// block's start. An inline extent's bytes begin 21 bytes later.
    pub data_offset: u32,
}

impl ExtentClaim {
    /// The logical address of an inline extent's stored bytes, inside its
    /// leaf. `None` for any other kind.
    pub fn inline_logical(&self) -> Option<u64> {
        (self.item.kind == ExtentType::Inline)
            .then(|| self.leaf + u64::from(self.data_offset) + INLINE_HEADER)
    }
}

/// Bytes of an inline extent item before its data.
const INLINE_HEADER: u64 = 21;

/// Everything a filesystem tree says about one inode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InodeRecord {
    pub ino: u64,
    /// The inode item, when one decoded.
    pub inode: Option<Inode>,
    /// Whether the leaf holding the inode item verified.
    pub inode_verified: bool,
    /// Every name that claims this inode.
    pub names: Vec<NameClaim>,
    /// Every extent item, in file order.
    pub extents: Vec<ExtentClaim>,
}

/// What [`collect_inodes`] found.
#[derive(Debug, Clone, Default)]
pub struct InodeScan {
    pub inodes: BTreeMap<u64, InodeRecord>,
    pub walk: WalkReport,
}

/// Parse the `(index, name)` pairs packed into an `INODE_REF` item.
fn inode_refs(data: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let mut refs = Vec::new();
    let mut at = 0usize;
    while at + 10 <= data.len() {
        let index = le64(data, at);
        let len = le16(data, at + 8) as usize;
        let Some(name) = data.get(at + 10..at + 10 + len) else {
            break;
        };
        refs.push((index, name.to_vec()));
        at += 10 + len;
    }
    refs
}

/// The record for `ino`, made empty when there is none yet.
fn record(inodes: &mut BTreeMap<u64, InodeRecord>, ino: u64) -> &mut InodeRecord {
    inodes.entry(ino).or_insert_with(|| InodeRecord {
        ino,
        ..InodeRecord::default()
    })
}

/// Walk a filesystem tree past its damage and gather every inode with
/// every claim made about it.
pub fn collect_inodes(source: &dyn BlockRead, layout: &Layout, root: RootPointer) -> InodeScan {
    let mut inodes: BTreeMap<u64, InodeRecord> = BTreeMap::new();
    let walk = walk_tree(source, layout, root, &mut |leaf| {
        let verified = leaf.is_verified();
        for item in leaf.items() {
            let Some(data) = leaf.item_data(item) else {
                continue;
            };
            let key = item.key;
            match key.key_type {
                INODE_ITEM_KEY => {
                    if let Ok(inode) = Inode::parse(data, key.objectid) {
                        let entry = record(&mut inodes, key.objectid);
                        if entry.inode.is_none() || (verified && !entry.inode_verified) {
                            entry.inode = Some(inode);
                            entry.inode_verified = verified;
                        }
                    }
                }
                INODE_REF_KEY => {
                    let entry = record(&mut inodes, key.objectid);
                    for (index, name) in inode_refs(data) {
                        entry.names.push(NameClaim {
                            parent: key.offset,
                            name,
                            index,
                            from_directory: false,
                            verified,
                        });
                    }
                }
                DIR_INDEX_KEY => {
                    if let Ok(entries) = parse_dir_items(data) {
                        for entry in entries.into_iter().filter(|e| e.is_inode()) {
                            record(&mut inodes, entry.ino).names.push(NameClaim {
                                parent: key.objectid,
                                name: entry.name,
                                index: key.offset,
                                from_directory: true,
                                verified,
                            });
                        }
                    }
                }
                EXTENT_DATA_KEY => {
                    if let Some(extent) = FileExtentItem::parse(key.offset, data) {
                        record(&mut inodes, key.objectid).extents.push(ExtentClaim {
                            item: extent,
                            verified,
                            leaf: leaf.logical,
                            leaf_mirror: leaf.mirror,
                            data_offset: (crate::btree::LEAF_DATA_OFFSET as u32) + item.offset,
                        });
                    }
                }
                _ => {}
            }
        }
        ControlFlow::Continue(())
    });
    for record in inodes.values_mut() {
        record.extents.sort_by_key(|e| e.item.file_offset);
    }
    InodeScan { inodes, walk }
}

/// What [`copy_file`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileCopy {
    /// Bytes written: the inode's size, or the end of its last extent when
    /// no inode item survived.
    pub length: u64,
    /// File ranges no extent covered, written as zeros: holes, or extents
    /// lost with a damaged leaf.
    pub uncovered: Vec<(u64, u64)>,
    /// Every extent whose bytes are not all what was written.
    pub faults: Vec<(u64, DataFault)>,
    /// Whether every extent item came from a verified leaf.
    pub extents_verified: bool,
    /// How the file's data sectors checked against the checksum tree.
    pub sectors: SectorTally,
}

/// Write a file's bytes to `sink` from its extent items, in file order,
/// zeros where nothing covers a range. The length is the inode's size when
/// the inode item survived.
pub fn copy_file(
    source: &dyn BlockRead,
    layout: &Layout,
    csums: Option<&CsumTree>,
    record: &InodeRecord,
    sink: &mut dyn io::Write,
) -> io::Result<FileCopy> {
    let size = record.inode.as_ref().map(|i| i.size).unwrap_or_else(|| {
        record
            .extents
            .iter()
            .map(|e| e.item.file_offset + e.item.num_bytes)
            .max()
            .unwrap_or(0)
    });
    let mut report = FileCopy {
        length: size,
        extents_verified: record.extents.iter().all(|e| e.verified),
        ..FileCopy::default()
    };
    let zeros = |sink: &mut dyn io::Write, mut n: u64| -> io::Result<()> {
        let block = [0u8; 4096];
        while n > 0 {
            let take = n.min(block.len() as u64) as usize;
            sink.write_all(&block[..take])?;
            n -= take as u64;
        }
        Ok(())
    };
    let mut at = 0u64;
    let count = record.extents.len();
    for (index, claim) in record.extents.iter().enumerate() {
        let item = &claim.item;
        if item.file_offset >= size {
            break;
        }
        if item.file_offset < at {
            continue;
        }
        if item.file_offset > at {
            report.uncovered.push((at, item.file_offset - at));
            zeros(sink, item.file_offset - at)?;
            at = item.file_offset;
        }
        let (bytes, faults, tally) =
            read_file_extent(source, layout, csums, item, index + 1 == count);
        report.sectors.add(tally);
        let take = (bytes.len() as u64).min(size - at) as usize;
        sink.write_all(&bytes[..take])?;
        at += take as u64;
        report
            .faults
            .extend(faults.into_iter().map(|f| (item.file_offset, f)));
    }
    if at < size {
        report.uncovered.push((at, size - at));
        zeros(sink, size - at)?;
    }
    Ok(report)
}
