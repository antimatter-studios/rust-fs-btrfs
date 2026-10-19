### Added

- **A single data block group converts to DUP (#586, part of #400).**
  `Filesystem::relocate_block_group` with `Some(ChunkProfile::Dup)` moves a
  single data group into a new DUP chunk, writing every moved sector to both
  copies, instead of refusing the conversion. Every other conversion, and any
  conversion of a metadata group, is still refused before anything is written.
