### Added

- **Unlink removes the last name held in an extended reference (#511).**
  Once the final `INODE_EXTREF` record has been removed, inode cleanup
  accepts its emptied reference item just as it accepts an emptied
  `INODE_REF`. The inode, its attributes and its inline or regular data
  are released in the same transaction. A reference item that still
  holds a name remains refused. The kernel-oracle regression in
  `tests/extref_namespace_kernel.rs` leaves only a spilled name on files
  with inline and regular data, then checks the ABI's unlink with
  `btrfs check`, `dump-tree` and a kernel directory listing.
