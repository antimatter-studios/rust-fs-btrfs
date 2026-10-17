### Added

- **Hard links past a full inode reference are filed in extended
  references (#425).** `fs_btrfs_link`, and a rename that files a name in
  a directory where the file's `INODE_REF` item is already full, refused
  the name once that item would outgrow a leaf. On a volume with the
  `EXTENDED_IREF` feature, the `mkfs.btrfs` default, they now file it in
  an `INODE_EXTREF` item under the hash of the parent and the name, as the
  kernel does; a volume without the feature still refuses it.
  `tests/link_extref_kernel.rs` links one file past its `INODE_REF`
  through the ABI, renames a spilled name within its directory, and has
  `btrfs check`, `dump-tree`'s extended references, the kernel's listing,
  link count and `inode-resolve`, and the kernel's own unlinks agree.
