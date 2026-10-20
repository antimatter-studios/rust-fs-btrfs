### Added

- **Unlink and rename remove a name held in an extended reference (#424).**
  `fs_btrfs_unlink` and `fs_btrfs_rename` refused a name a file keeps in
  an `INODE_EXTREF` item, where the kernel files the hard links its
  `INODE_REF` for one directory cannot hold, so such a file could never
  lose those links. They now take the record out of the extended
  reference, deleting the item once it is empty.
  `tests/extref_namespace_kernel.rs` has the kernel spill a file's links
  into extended references, unlinks one spilled name and moves another to
  a second directory through the ABI, and has `btrfs check`, the kernel's
  listing and link count, and `btrfs inspect-internal inode-resolve`
  agree. Adding a name past a full `INODE_REF` is still refused.
