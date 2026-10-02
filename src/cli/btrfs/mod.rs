//! The Btrfs tools: what this repository fills the shared contract in
//! with. Everything filesystem-specific lives here, and nothing here is
//! plumbing (that is `fs_core::cli`, in am-fs-core).
//!
//! One tool, `fs.btrfs`. There is no `mkfs.btrfs` (nothing in this crate
//! builds an initial layout) and no `fsck.btrfs` (it has no checker), so
//! neither name is linked: a missing name reads as "not shipped", where a
//! name that answered "not implemented" would shadow btrfs-progs' own.

pub mod device;
pub mod fs;
