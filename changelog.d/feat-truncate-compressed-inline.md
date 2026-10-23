### Added

- **Truncate cuts a compressed inline extent short (#423).**
  `Filesystem::truncate`, `fs_btrfs_truncate` and `fs.btrfs truncate`
  refused to cut a file whose data is a compressed inline extent, which a
  volume mounted with `compress` holds for many small files. They now
  decode the extent and write the bytes kept back as an uncompressed inline
  extent, so no stale decoded bytes stay past the new end.
  `tests/truncate_kernel.rs` has the kernel write zlib and zstd compressed
  inline files, cuts them through the ABI, and has `btrfs check`, the
  kernel's size and SHA-256, and `btrfs check` after a kernel append agree.
  An encrypted or otherwise encoded inline extent is still refused.
