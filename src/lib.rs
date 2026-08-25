//! rust-fs-btrfs — pure-Rust Btrfs filesystem driver.
//!
//! Exposes a stable C ABI (`fs_btrfs_*`) so FFI consumers (Swift/C/Go/…)
//! can link `libfs_btrfs.a` and `#include "fs_btrfs.h"`.
//!
//! # Byte order
//!
//! Btrfs is **little-endian on disk** on every host, checksums included.
//! Every on-disk integer in this crate is decoded with `from_le_bytes`.
//! That is worth stating up front because the sister XFS driver is
//! big-endian *except* for its CRC, and carrying either half of that
//! habit across produces a reader that agrees with its own test fixtures
//! and with nothing else.
//!
//! # Reading order
//!
//! Btrfs addresses everything — tree roots, node children, file extents —
//! in a single flat logical address space, and the table that translates
//! it lives inside the filesystem it describes. Bootstrapping therefore
//! runs in a fixed order:
//!
//! 1. Read the superblock from a fixed physical offset
//!    ([`superblock::SUPER_OFFSETS`]) and verify its checksum.
//! 2. Build the bootstrap address map from the superblock's embedded
//!    `sys_chunk_array` ([`chunk::ChunkMap::bootstrap`]).
//! 3. Use that map to reach the chunk tree, and extend the map with
//!    every chunk item it holds.
//! 4. Only then is `root` — and through it the rest of the volume —
//!    readable.
//!
//! Steps 1 and 2 are what this crate implements today.
//!
//! # Status
//!
//! Read path first. Unit tests here are deliberately treated as
//! necessary-but-not-sufficient: a fixture this crate builds itself
//! cannot catch this crate misreading the on-disk format, because the
//! misreading would be baked into both sides. Correctness against real
//! media is established by cross-validating against `mkfs.btrfs` output.
//!
//! Architecture:
//! - [`error`] — driver error type, mapped to errno by the C ABI
//! - [`superblock`] — superblock parse + validation, checksum algorithms,
//!   feature gating
//! - [`chunk`] — chunk items and the logical-to-physical address map

#![deny(unsafe_op_in_unsafe_fn)]

pub mod btree;
pub mod capi;
pub mod chunk;
pub mod dir;
pub mod error;
pub mod fs;
pub mod inode;
pub mod superblock;

pub use chunk::{Chunk, ChunkMap, ChunkProfile, Mapping};
pub use error::{Error, Result};
pub use fs::Filesystem;
pub use superblock::{ChecksumType, Superblock};
