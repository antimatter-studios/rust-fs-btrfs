### Added

- **Truncate grows a file on a volume without the no-holes feature
  (#420).** `Filesystem::truncate`, `fs_btrfs_truncate` and
  `fs.btrfs truncate` refused to grow a file there, because every range of
  such a file up to its size must have an item. They now record each part
  of the new range that no item covers as a hole item, as the kernel does,
  in the same transaction as the new size. `tests/truncate_kernel.rs` has
  `btrfs check` find the volume clean, `dump-tree` find the items tiling
  each file, and the kernel read the grown files back.
