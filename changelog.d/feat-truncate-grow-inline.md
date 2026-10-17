### Added

- **Truncate grows a file that holds inline data (#422).**
  `Filesystem::truncate`, `fs_btrfs_truncate` and `fs.btrfs truncate`
  refused to grow such a file, because an inline extent cannot stay shorter
  than its file. They now move the inline data into a regular extent at
  offset 0, written copy-on-write with zeros to the end of its sector and
  checksummed, as the kernel does, before moving the size. A growth inside
  that sector and one past it are both covered. `tests/truncate_kernel.rs`
  has the kernel write inline files, grows them through the ABI, and has
  `btrfs check`, its data-checksum pass, `dump-tree` (no inline item left),
  the kernel's size and SHA-256, and `btrfs check` after a kernel append
  agree. A volume without the no-holes feature still refuses it.
