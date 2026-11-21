//! Read-only primitives for a damaged volume.
//!
//! A mount ([`crate::fs::Filesystem::mount`]) needs a volume whose
//! structures verify, and stops at the first one that does not. Salvage
//! needs the opposite: everything that can still be read, with a verdict
//! on each structure instead of a refusal. These primitives are the
//! PrimitiveAPI layer of `docs/three-layers.md`, and the salvage verbs are
//! built from them.
//!
//! **Read-only by type.** Every function here takes the source as
//! `&dyn BlockRead`, a positioned-read trait with no write path. None takes
//! a device that can be written, a path, or a handle that could become one.
//!
//! **Damage is evidence.** A structure that fails a check is returned with
//! its verdict, never dropped for it. An `Err` from these functions means
//! the source could not be asked at all, never that what it held was
//! damaged.

pub mod chunks;
pub mod files;
pub mod roots;
pub mod scan;
pub mod superblock;
pub mod tree;

pub use chunks::{build_chunk_map, ChunkScan, ChunkSource, RejectedChunk};
pub use files::{
    collect_inodes, copy_file, read_data, read_file_extent, DataFault, ExtentClaim, ExtentType,
    FileCopy, FileExtentItem, InodeRecord, InodeScan, NameClaim,
};
pub use roots::{read_root_items, root_tree_candidates, RootItem, RootSource, RootTreeCandidate};
pub use scan::{metadata_ranges, physical_to_logical, scan_tree_blocks, FoundBlock, ScanSummary};
pub use superblock::{best_copy, superblock_copies, CopyVerdict, SuperblockCopy};
pub use tree::{
    decode_block, read_block, walk_tree, BlockFault, CopyAttempt, DamagedBlock, Expect, Layout,
    RootPointer, SalvagedBlock, SalvagedBody, WalkReport,
};
