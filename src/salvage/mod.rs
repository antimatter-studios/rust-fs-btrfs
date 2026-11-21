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

pub mod superblock;

pub use superblock::{best_copy, superblock_copies, CopyVerdict, SuperblockCopy};
