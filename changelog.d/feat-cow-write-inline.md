### Added

- **A copy-on-write write inside an uncompressed inline extent (#437).**
  It was refused. A write that lies within a small file's inline data
  rewrites that item with the new bytes, its size unchanged, and the file
  stays inline, as the kernel leaves it; the inode's times, `transid` and
  `sequence` move as for any write.
