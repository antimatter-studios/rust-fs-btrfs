//! Tree blocks found by scanning the raw device, as `btrfs-find-root`
//! finds them.
//!
//! When neither the current root tree nor any backup root leads anywhere,
//! the blocks are still on the device. Each carries this filesystem's fsid,
//! its own logical address, its generation, its owner tree and its level,
//! under a checksum, so a scan of node-sized windows finds every intact
//! one without any tree to reach it through.

use super::tree::Layout;
use crate::btree::{Header, HEADER_SIZE};
use crate::chunk::ChunkProfile;
use crate::superblock::CSUM_SIZE;
use fs_core::BlockRead;
use std::ops::ControlFlow;

/// Bytes read from the source per request.
const SCAN_CHUNK: u64 = 1 << 20;

/// A node-sized window that carries this filesystem's fsid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundBlock {
    /// Where the window starts on the source.
    pub physical: u64,
    /// The logical address the chunk map gives this physical offset, when
    /// a chunk's stripe covers it.
    pub mapped_logical: Option<u64>,
    pub header: Header,
    /// The checksum over the window verifies.
    pub checksum_ok: bool,
}

impl FoundBlock {
    /// An intact block that sits where its header says: the checksum
    /// verifies and the chunk map puts its own address here.
    pub fn is_consistent(&self) -> bool {
        self.checksum_ok && self.mapped_logical == Some(self.header.bytenr)
    }
}

/// The outcome of a [`scan_tree_blocks`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSummary {
    /// Windows that carried the fsid and went to the sink.
    pub found: u64,
    /// `(offset, length)` ranges that could not be read.
    pub unreadable: Vec<(u64, u64)>,
    /// Set when the sink stopped the scan: the first offset not scanned.
    pub stopped_at: Option<u64>,
}

/// The logical address of `physical` on the source, through a chunk whose
/// stripes each hold a whole copy of it (single, DUP, RAID1). Striped
/// profiles are not inverted.
pub fn physical_to_logical(layout: &Layout, physical: u64) -> Option<u64> {
    layout.chunks.chunks().iter().find_map(|chunk| {
        let whole_copies = matches!(
            chunk.profile().ok()?,
            ChunkProfile::Single
                | ChunkProfile::Dup
                | ChunkProfile::Raid1
                | ChunkProfile::Raid1c3
                | ChunkProfile::Raid1c4
        );
        if !whole_copies {
            return None;
        }
        chunk.stripes.iter().find_map(|stripe| {
            (stripe.devid == layout.devid
                && physical >= stripe.offset
                && physical - stripe.offset < chunk.length)
                .then(|| chunk.logical + (physical - stripe.offset))
        })
    })
}

/// The `(offset, length)` ranges of the source the metadata and system
/// chunks occupy, where tree blocks live. Scanning these instead of the
/// whole device is what `btrfs-find-root` does.
pub fn metadata_ranges(layout: &Layout) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = layout
        .chunks
        .chunks()
        .iter()
        .filter(|chunk| chunk.is_metadata() || chunk.is_system())
        .flat_map(|chunk| {
            let per_stripe = match chunk.profile() {
                Ok(ChunkProfile::Single | ChunkProfile::Dup)
                | Ok(ChunkProfile::Raid1 | ChunkProfile::Raid1c3 | ChunkProfile::Raid1c4) => {
                    chunk.length
                }
                _ => chunk.length / u64::from(chunk.num_stripes.max(1)),
            };
            chunk
                .stripes
                .iter()
                .filter(|stripe| stripe.devid == layout.devid)
                .map(move |stripe| (stripe.offset, per_stripe))
                .collect::<Vec<_>>()
        })
        .collect();
    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

/// Scan `[offset, offset + length)` of the source in node-sized windows at
/// node-size alignment, and hand every window that carries the layout's
/// fsid to `sink`, intact or not.
///
/// An unreadable span is retried in node-sized pieces; pieces that still
/// fail are listed in the summary and the scan goes on.
pub fn scan_tree_blocks(
    source: &dyn BlockRead,
    layout: &Layout,
    offset: u64,
    length: u64,
    sink: &mut dyn FnMut(FoundBlock) -> ControlFlow<()>,
) -> ScanSummary {
    let nodesize = u64::from(layout.geometry.nodesize);
    let end = offset.saturating_add(length).min(source.size_bytes());
    let mut at = offset.div_ceil(nodesize) * nodesize;
    let mut summary = ScanSummary::default();
    let mut buffer = vec![0u8; SCAN_CHUNK as usize];
    while at + nodesize <= end {
        let span = (SCAN_CHUNK / nodesize * nodesize).min((end - at) / nodesize * nodesize);
        let chunk = &mut buffer[..span as usize];
        let pieces: Vec<(u64, bool)> = if source.read_at(at, chunk).is_ok() {
            (0..span / nodesize)
                .map(|i| (at + i * nodesize, true))
                .collect()
        } else {
            (0..span / nodesize)
                .map(|i| {
                    let window = &mut chunk[(i * nodesize) as usize..((i + 1) * nodesize) as usize];
                    (
                        at + i * nodesize,
                        source.read_at(at + i * nodesize, window).is_ok(),
                    )
                })
                .collect()
        };
        for (physical, readable) in pieces {
            if !readable {
                match summary.unreadable.last_mut() {
                    Some(last) if last.0 + last.1 == physical => last.1 += nodesize,
                    _ => summary.unreadable.push((physical, nodesize)),
                }
                continue;
            }
            let start = (physical - at) as usize;
            let window = &chunk[start..start + nodesize as usize];
            if window[0x20..0x30] != layout.geometry.fsid {
                continue;
            }
            let Ok(header) = Header::parse(&window[..HEADER_SIZE]) else {
                continue;
            };
            let found = FoundBlock {
                physical,
                mapped_logical: physical_to_logical(layout, physical),
                checksum_ok: layout
                    .geometry
                    .csum_type
                    .verify(&window[CSUM_SIZE..], &header.csum),
                header,
            };
            summary.found += 1;
            if sink(found).is_break() {
                summary.stopped_at = Some(physical + nodesize);
                return summary;
            }
        }
        at += span;
    }
    summary
}
