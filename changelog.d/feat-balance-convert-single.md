### Added

- **A DUP data block group converts to single (#587, part of #400).**
  `Filesystem::relocate_block_group` with `Some(ChunkProfile::Single)` moves a
  DUP data group into a new single chunk, reading each extent from whichever
  copy verifies, and removes the DUP chunk with both its device extents,
  instead of refusing the group. DUP metadata groups, and relocating a DUP
  data group without that conversion, are still refused before anything is
  written.
