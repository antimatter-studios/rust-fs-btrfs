### Added

- **Every tree block in a metadata block group can be copied out of it
  (#578).** `Filesystem::copy_tree_blocks_out_of` makes each tree block whose
  address lies in the group dirty, with the group kept out of the tree block
  allocator, and commits one transaction: each block and its path to its
  tree's root are rewritten elsewhere, with parents, `ROOT_ITEM`s, the
  superblock's root pointers, extent records, the free-space tree and both
  groups' `used` following. The group itself stays for a relocation to
  remove. A kernel test has `btrfs check` find the volume clean and
  `dump-tree` list no tree block left in the group.
