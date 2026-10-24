### Added

- **Truncate grows a file that ends partway through a sector (#421).**
  `Filesystem::truncate`, `fs_btrfs_truncate` and `fs.btrfs truncate`
  refused to grow such a file, because the bytes between its old end and
  the sector boundary may hold stale data. They now rewrite that sector
  copy-on-write with zeros past the old end, as the kernel does, before
  moving the size. `tests/truncate_kernel.rs` grows files cut short by the
  kernel and through the ABI, and has `btrfs check`, its data-checksum
  pass, the kernel's size and SHA-256 and a scrub agree. A volume without
  the no-holes feature still refuses it.
