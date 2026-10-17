### Added

- **An empty data block group relocated into a new chunk (#397, part of
  #304).** `Filesystem::relocate_block_group(start, None)` allocates a
  chunk of the same profile (its chunk item, device extent, empty block
  group item, free-space record covering it, and the device's
  `bytes_used` raised in the chunk tree and the superblock) in one
  committed transaction, then removes the old group in another: the two
  steps every balance is built from. Single profile, one device and a
  free-space tree only; a group holding extents, a metadata or system
  group, a conversion, and a block group tree volume are refused before
  anything is written.
