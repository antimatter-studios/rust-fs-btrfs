### Added

- **A full root node splits, and its tree grows a level (#513).** When the
  leaves a change splits leave a tree's root node with more key pointers than
  one block holds, that node splits in two under a new root node a level up,
  with extent records at the right levels and the tree's `ROOT_ITEM` level
  raised to match. A directory with a few hundred names on a 4 KiB node used
  to stop taking names. A full node below the root, and growing the root or
  chunk tree, are still refused. `tests/node_split_kernel.rs` adds names
  through `fs_btrfs_create` until tree 5's root splits, and has `btrfs check`
  find it clean, `dump-tree` show a level-2 root over two level-1 nodes, and
  the kernel list every name. **Breaking:** `transaction::Split` gains
  `level`, the height of the block that splits.
